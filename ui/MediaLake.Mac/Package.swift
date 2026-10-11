// swift-tools-version: 5.9
import PackageDescription

// The Rust core is linked as a static library built by
//   cargo rustc --release -p medialake-ffi --crate-type staticlib
// (scripts/macos/build-release.sh does this). MEDIALAKE_FFI_LIB points at another build, e.g. a debug one.
import Foundation
let root = Context.packageDirectory
let ffiLib = ProcessInfo.processInfo.environment["MEDIALAKE_FFI_LIB"] ?? "\(root)/../../target/release/libmedialake_core_ffi.a"

let package = Package(
    name: "MediaLake",
    platforms: [.macOS(.v14)],
    products: [.executable(name: "MediaLake", targets: ["MediaLake"])],
    targets: [
        .target(name: "CMediaLakeCore", path: "Sources/CMediaLakeCore"),
        .executableTarget(
            name: "MediaLake",
            dependencies: ["CMediaLakeCore"],
            path: "Sources/MediaLake",
            linkerSettings: [
                .unsafeFlags([ffiLib]),
                // `cargo rustc … -- --print native-static-libs` lists what the Rust side needs
                .linkedLibrary("c++"),
                .linkedLibrary("iconv"),
                .linkedLibrary("resolv"),
                .linkedFramework("Security"),
                .linkedFramework("CoreFoundation"),
                .linkedFramework("CoreServices"),
                .linkedFramework("SystemConfiguration"),
                .linkedFramework("AVFoundation"),
                .linkedFramework("AVFAudio"),
                .linkedFramework("CoreImage"),
                .linkedFramework("QuartzCore"),
                .linkedFramework("MediaToolbox"),
                .linkedFramework("CoreMedia"),
                .linkedFramework("CoreVideo"),
                .linkedFramework("CoreAudio"),
                .linkedFramework("ImageIO"),
                .linkedFramework("CoreGraphics"),
                .linkedFramework("Metal"),
                .linkedFramework("Foundation"),
            ]
        ),
    ]
)
