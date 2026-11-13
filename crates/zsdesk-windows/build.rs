fn main() {
    println!("cargo:rerun-if-changed=native/shell_menu.cpp");
    println!("cargo:rerun-if-changed=native/MenuSelectionSnapshot.h");
    println!("cargo:rerun-if-changed=native/CommandAgility.h");
    println!("cargo:rerun-if-changed=native/MenuCommandBridge.h");
    println!("cargo:rerun-if-changed=native/MenuCommandDispatch.h");
    println!("cargo:rerun-if-changed=native/MixedItemMenu.h");
    println!("cargo:rerun-if-changed=native/MenuModelFilter.h");
    println!("cargo:rerun-if-changed=native/StandaloneMenuHost.h");
    println!("cargo:rerun-if-changed=native/minhook");
    if std::env::var("TARGET").as_deref() == Ok("x86_64-pc-windows-msvc") {
        cc::Build::new().files(["native/minhook/src/buffer.c", "native/minhook/src/hook.c",
                "native/minhook/src/trampoline.c", "native/minhook/src/hde/hde64.c"])
            .include("native/minhook/include").define("UNICODE", None).define("_UNICODE", None)
            .compile("zsdesk_minhook");
        cc::Build::new().cpp(true).file("native/shell_menu.cpp").include("native/minhook/include")
            .define("UNICODE", None).define("_UNICODE", None)
            .flag("/std:c++17").flag("/EHsc").flag("/utf-8").compile("zsdesk_menu");
        for lib in ["ole32", "uuid", "bcrypt", "user32", "comctl32", "runtimeobject", "shell32"] {
            println!("cargo:rustc-link-lib={lib}");
        }
    }
}
