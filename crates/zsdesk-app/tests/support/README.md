# Native menu extension fixture

`menu_extension.cpp` implements a minimal `IExplorerCommand` provider for the native menu regression. It writes the selected absolute Shell identities to the file named by `ZSDESK_TEST_COMMAND_MARKER` using `CREATE_NEW`, so duplicate invocation fails instead of overwriting evidence.

Build the DLL in an external test output directory using the x64 Visual C++ tools:

```text
cl /nologo /std:c++17 /EHsc /O2 /MT /LD /DUNICODE /D_UNICODE menu_extension.cpp /link /EXPORT:DllGetClassObject /EXPORT:DllCanUnloadNow ole32.lib shell32.lib shlwapi.lib uuid.lib
```

Set `ZSDESK_MENU_EXTENSION_PROBE` to the DLL's absolute path and `ZSDESK_NATIVE_TEST_OUTPUT` to an isolated writable output directory, then run `native_modern_fence_extension_executes_exact_selection` alone with `--ignored --test-threads=1`. The test temporarily registers this fixture's CLSID and inserts the command through the verified native command bridge; it does not install an AppX package or run a third-party editor.

Property-sheet verification requires the interactive desktop. Set `ZSDESK_MENU_TEST_INPUT_DESKTOP=1` for `native_modern_fence_properties_execute_once`; it invokes only the owned fixture files and closes their property sheets. Other menu tests use isolated desktops or a private window station. Do not run tests sharing a window station or clipboard concurrently.
