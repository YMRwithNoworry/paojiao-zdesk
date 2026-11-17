fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    println!("cargo:rerun-if-changed=native/early_window_hook.cpp");
    if std::env::var("TARGET").as_deref()==Ok("x86_64-pc-windows-msvc") {
        let out=std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
        let source=std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("native/early_window_hook.cpp");
        let fingerprint=std::fs::read(&source).expect("hook source").iter().fold(0xcbf29ce484222325u64,|hash,byte|(hash^u64::from(*byte)).wrapping_mul(0x100000001b3));
        let name=format!("ZSDeskWindowHook-{fingerprint:016x}.dll");
        let dll=out.join(&name);
        println!("cargo:rustc-env=ZSDESK_WINDOW_HOOK={name}");
        let tool=cc::Build::new().cpp(true).get_compiler();
        let status=tool.to_command().current_dir(&out).args(["/nologo","/O2","/GS-","/GR-","/Zl","/LD","/utf-8"])
            .arg(source).arg(format!("/Fe:{}",dll.display())).args(["/link","/NODEFAULTLIB","/ENTRY:DllMain","user32.lib","kernel32.lib"]).status().expect("compile early window hook");
        assert!(status.success(),"early window hook compilation failed");
        let profile=out.ancestors().nth(3).expect("target profile");
        std::fs::copy(&dll,profile.join(&name)).expect("publish hook DLL");
        std::fs::write(profile.join("ZSDeskWindowHook.name"),name).expect("publish hook identity");
    }

    println!("cargo:rerun-if-changed=resources/zsdesk.rc");
    println!("cargo:rerun-if-changed=resources/zsdesk.ico");
    println!("cargo:rerun-if-changed=resources/zsdesk.exe.manifest");
    embed_resource::compile("resources/zsdesk.rc", embed_resource::NONE)
        .manifest_required()
        .expect("failed to embed the Windows application manifest");
}
