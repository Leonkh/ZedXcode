// Tuist manifest for the tuist-shaped layout fixture.
//
// Never executed in CI. The committed MyApp.xcworkspace and MyApp.xcodeproj
// stand in for the output of `tuist generate --no-open` and were written by
// hand to describe the same app; regenerate deliberately if they must match.
import ProjectDescription

let project = Project(
    name: "MyApp",
    targets: [
        .target(
            name: "MyApp",
            destinations: [.iPhone],
            product: .app,
            bundleId: "com.example.MyApp",
            deploymentTargets: .iOS("17.0"),
            infoPlist: nil,
            sources: ["MyApp/**/*.swift"],
            settings: .settings(base: [
                "CODE_SIGN_STYLE": "Automatic",
                "GENERATE_INFOPLIST_FILE": "YES",
                "INFOPLIST_KEY_UILaunchScreen_Generation": "YES",
                "SWIFT_VERSION": "5.0",
            ])
        ),
    ],
    schemes: [
        .scheme(
            name: "MyApp",
            shared: true,
            buildAction: .buildAction(targets: ["MyApp"]),
            runAction: .runAction(
                configuration: .debug,
                customLLDBInitFile: "MyApp/run.lldbinit",
                arguments: .arguments(
                    environmentVariables: ["MYAPP_FLAG": "1"],
                    launchArguments: [.launchArgument(name: "-MyAppArg YES", isEnabled: true)]
                )
            )
        ),
    ]
)
