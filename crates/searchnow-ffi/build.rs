fn main() {
    csbindgen::Builder::default()
        .input_extern_file("src/lib.rs")
        .csharp_dll_name("searchnow_core_ffi")
        .csharp_namespace("SearchNow.Native")
        .csharp_class_name("NativeMethods")
        .csharp_use_function_pointer(true)
        .generate_csharp_file("../../ui/SearchNow.App/Native/NativeMethods.g.cs")
        .unwrap();
}
