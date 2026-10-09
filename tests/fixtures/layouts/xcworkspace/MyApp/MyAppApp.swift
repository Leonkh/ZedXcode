import SwiftUI
import UIKit

/// The neutral fixture app that ZedXcode's checks build, launch and debug.
///
/// Console lines, in order (the simulator smoke test reads them):
///
///     MyApp launched
///     env MYAPP_FLAG=<value, or "unset">
///     arg -MyAppArg=<true|false>
///     tick 1
///     tick 2
///     ...                      one line a second
///
/// The scheme sets `MYAPP_FLAG=1` and the argument `-MyAppArg YES`, so a launch
/// that applies the scheme prints `env MYAPP_FLAG=1` and `arg -MyAppArg=true`.
/// Launched with `-MTCProbe` (after the other arguments), the app also calls
/// UIKit once off the main thread so the Main Thread Checker has something to
/// report. Output goes through plain `print`, never flushed by hand: whether it
/// shows up promptly depends on how the app is launched, which is under test.
@main
struct MyAppApp: App {
    private let greeting = Greeting(appName: "MyApp")

    init() {
        let environment = ProcessInfo.processInfo.environment
        print(greeting.launchLine)
        print("env MYAPP_FLAG=\(environment["MYAPP_FLAG"] ?? "unset")")
        print("arg -MyAppArg=\(UserDefaults.standard.bool(forKey: "MyAppArg"))")
        Self.startTicking()
    }

    var body: some Scene {
        WindowGroup {
            ContentView(greeting: greeting)
        }
    }

    /// Prints `tick N` once a second, starting at 1, for as long as the app runs.
    private static func startTicking() {
        Task { @MainActor in
            var tick = 0
            while true {
                try? await Task.sleep(nanoseconds: 1_000_000_000)
                tick += 1
                print("tick \(tick)")
            }
        }
    }
}

struct ContentView: View {
    let greeting: Greeting

    var body: some View {
        VStack(spacing: 12) {
            Text(greeting.title)
        }
        .padding()
        .onAppear {
            MainThreadCheckerProbe.runIfRequested()
        }
    }
}

/// `-MTCProbe`: one UIKit call off the main thread, once per launch.
///
/// The call goes through `perform(_:)` so the file compiles without
/// concurrency diagnostics; the Main Thread Checker still sees
/// `-[UIView setNeedsLayout]` running on a background queue.
@MainActor
enum MainThreadCheckerProbe {
    private static var didRun = false

    static func runIfRequested() {
        guard !didRun, ProcessInfo.processInfo.arguments.contains("-MTCProbe") else { return }
        didRun = true
        let view = UIView()
        print("probe: calling UIKit off the main thread")
        DispatchQueue.global(qos: .utility).async {
            _ = view.perform(#selector(UIView.setNeedsLayout))
        }
    }
}
