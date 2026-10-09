/// The app's greeting. MyAppApp.swift uses it, so renaming `Greeting` (or one
/// of its members) is a rename across two files.
struct Greeting {
    let appName: String

    /// The first console line: "MyApp launched".
    var launchLine: String { "\(appName) launched" }

    /// The text on the app's only screen.
    var title: String { "Hello from \(appName)" }
}
