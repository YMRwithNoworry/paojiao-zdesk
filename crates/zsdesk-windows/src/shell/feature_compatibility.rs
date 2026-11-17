//! Read-only compatibility gate for the verified native menu presenter.
use windows::{
    core::{w, PCSTR},
    Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress},
};

// These runtime features select newer menu layouts inside the same image.
// Default is not evidence that the older private presenter layout is active.
const NEW_MENU_FEATURES: [u32; 3] = [49_453_572, 60_813_048, 61_934_903];
const RUNTIME_CONFIGURATION: u32 = 1;
const EXPLICITLY_DISABLED: u32 = 1;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FeatureConfiguration {
    feature_id: u32,
    compact_state: u32,
    _variant_payload: u32,
}

#[derive(Clone, Copy)]
struct FeatureReply {
    status: i32,
    change_stamp: u64,
    configuration: FeatureConfiguration,
}

type QueryFeatureConfiguration = unsafe extern "system" fn(
    u32,
    u32,
    *mut u64,
    *mut FeatureConfiguration,
) -> i32;

fn query_runtime_features() -> Option<[FeatureReply; 3]> {
    // ntdll is already loaded. No additional library or system configuration
    // is installed, and unavailable exports leave the presenter disabled.
    let module = unsafe { GetModuleHandleW(w!("ntdll.dll")) }.ok()?;
    let address = unsafe {
        GetProcAddress(module, PCSTR(c"RtlQueryFeatureConfiguration".as_ptr().cast()))
    }?;
    // The native query takes an ID, store kind, change stamp and a three-u32
    // configuration record, and returns a 32-bit NTSTATUS.
    let query: QueryFeatureConfiguration = unsafe { std::mem::transmute(address) };
    Some(NEW_MENU_FEATURES.map(|feature_id| {
        let mut configuration = FeatureConfiguration::default();
        let mut change_stamp = 0;
        let status = unsafe {
            query(feature_id, RUNTIME_CONFIGURATION, &mut change_stamp, &mut configuration)
        };
        FeatureReply { status, change_stamp, configuration }
    }))
}

fn compatible(image_matches: bool, replies: Option<&[FeatureReply; 3]>) -> bool {
    if !image_matches { return false; }
    let Some(replies) = replies else { return false; };
    let stamp = replies[0].change_stamp;
    NEW_MENU_FEATURES.iter().zip(replies).all(|(&feature_id, reply)| {
        // Accept only a complete successful reply for the requested feature.
        // Reject observations spanning a runtime configuration change.
        reply.status == 0
            && reply.change_stamp == stamp
            && reply.configuration.feature_id == feature_id
            && ((reply.configuration.compact_state >> 4) & 3) == EXPLICITLY_DISABLED
    })
}

pub(super) fn supports_old_layout(image_matches: bool) -> bool {
    if !image_matches { return false; }
    compatible(image_matches, query_runtime_features().as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disabled() -> [FeatureReply; 3] {
        NEW_MENU_FEATURES.map(|feature_id| FeatureReply {
            status: 0,
            change_stamp: 7,
            configuration: FeatureConfiguration {
                feature_id,
                compact_state: EXPLICITLY_DISABLED << 4,
                ..Default::default()
            },
        })
    }

    #[test]
    fn native_layout_requires_the_known_image_and_all_feature_replies() {
        assert_eq!(std::mem::size_of::<FeatureConfiguration>(), 12);
        assert!(compatible(true, Some(&disabled())));
        assert!(!compatible(false, Some(&disabled())));
        assert!(!compatible(true, None));
    }

    #[test]
    fn default_enabled_reserved_failed_and_incomplete_feature_replies_are_rejected() {
        for index in 0..NEW_MENU_FEATURES.len() {
            for state in [0, 2, 3] {
                let mut replies = disabled();
                replies[index].configuration.compact_state = state << 4;
                assert!(!compatible(true, Some(&replies)), "feature {index}, state {state}");
            }
            for status in [1, -1] {
                let mut replies = disabled();
                replies[index].status = status;
                assert!(!compatible(true, Some(&replies)), "feature {index}, status {status}");
            }
            for returned_id in [0, NEW_MENU_FEATURES[(index + 1) % NEW_MENU_FEATURES.len()]] {
                let mut replies = disabled();
                replies[index].configuration.feature_id = returned_id;
                assert!(!compatible(true, Some(&replies)), "feature {index}, returned ID {returned_id}");
            }
        }
    }

    #[test]
    fn feature_configuration_changes_during_a_query_are_rejected() {
        for index in 0..NEW_MENU_FEATURES.len() {
            let mut replies = disabled();
            replies[index].change_stamp += 1;
            assert!(!compatible(true, Some(&replies)));
        }
    }

    #[test]
    fn disabled_feature_metadata_does_not_enable_a_new_layout() {
        let mut replies = disabled();
        for reply in &mut replies {
            reply.configuration.compact_state |= 8 | (3 << 8) | (1 << 16);
            reply.configuration._variant_payload = 123;
        }
        assert!(compatible(true, Some(&replies)));
    }
}
