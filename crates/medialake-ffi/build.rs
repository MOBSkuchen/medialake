fn main() {
    csbindgen::Builder::default()
        .input_extern_file("src/lib.rs")
        .csharp_dll_name("medialake_core_ffi")
        .csharp_namespace("MediaLake.Native")
        .csharp_class_name("NativeMethods")
        .csharp_use_function_pointer(true)
        .generate_csharp_file("../../ui/MediaLake.App/Native/NativeMethods.g.cs")
        .unwrap();
}
