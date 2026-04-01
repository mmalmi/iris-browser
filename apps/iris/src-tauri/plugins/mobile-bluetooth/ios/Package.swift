// swift-tools-version:5.3

import PackageDescription

let package = Package(
  name: "tauri-plugin-iris-mobile-bluetooth",
  platforms: [
    .macOS(.v10_13),
    .iOS(.v13),
  ],
  products: [
    .library(
      name: "tauri-plugin-iris-mobile-bluetooth",
      type: .static,
      targets: ["tauri-plugin-iris-mobile-bluetooth"])
  ],
  dependencies: [
    .package(name: "Tauri", path: "../.tauri/tauri-api")
  ],
  targets: [
    .target(
      name: "tauri-plugin-iris-mobile-bluetooth",
      dependencies: [
        .byName(name: "Tauri")
      ],
      path: "Sources")
  ]
)
