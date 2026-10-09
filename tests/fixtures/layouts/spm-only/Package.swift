// swift-tools-version:5.9
// A plain Swift package with no Xcode project or workspace: Xcode actions must
// find nothing to do here.
import PackageDescription

let package = Package(
    name: "MyKit",
    products: [
        .library(name: "MyKit", targets: ["MyKit"]),
    ],
    targets: [
        .target(name: "MyKit"),
    ]
)
