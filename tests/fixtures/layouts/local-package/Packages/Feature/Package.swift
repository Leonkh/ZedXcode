// swift-tools-version:5.9
// A minimal local package, referenced by MyApp.xcworkspace and linked by MyApp.
import PackageDescription

let package = Package(
    name: "Feature",
    platforms: [.iOS(.v17)],
    products: [
        .library(name: "Feature", targets: ["Feature"]),
    ],
    targets: [
        .target(name: "Feature"),
    ]
)
