use toolboks_addon_sdk::TutorialStep as S;
pub(super) fn steps() -> Vec<S> {
    [
        ("0", "table", "Debloater · privacy and advertising", "These entries describe optional Windows changes. Read what each change does, review its current state, and select only what you want. Recommendations are not applied automatically."),
        ("0", "inspector", "Review and apply a change", "The inspector explains the selected item, what it changes, and available apply or undo actions. Review the plan and restore-point option before applying changes. Some actions need administrator permission; the tour does not apply any."),
        ("1", "table", "Apps · installed packages", "This page lists supported installed application packages. Search and inspect the package identity before choosing to remove it; similarly named entries may serve different users or system functions."),
        ("1", "inspector", "Apps · removal scope", "The inspector explains the selected app and the available removal options. Removing an app can remove functionality; reinstalling may require the Store or its original installer. The walkthrough never removes a package."),
        ("2", "table", "Startup · what runs at sign-in", "Startup entries show programs configured to launch when you sign in, grouped by their source. Disabling startup does not uninstall the application or stop a process that is already running."),
        ("2", "inspector", "Startup · inspect and toggle", "Review the command, publisher and startup source before enabling or disabling an entry. Keep entries needed by your hardware or workflow. Changes occur only when you explicitly choose an action outside the tour."),
        ("3", "table", "Tasks · scheduled background work", "Scheduled tasks run in response to triggers or on a schedule. The grouped entries show supported tasks and current status. Read the explanation rather than disabling tasks just because they run in the background."),
        ("3", "inspector", "Tasks · effects and undo", "The inspector describes a task and available disable or undo controls. A disabled task no longer runs from its normal triggers. Review the tradeoffs and permission requirements before changing it."),
        ("4", "table", "Services · background components", "Windows services support system features and applications. This page focuses on supported optional services and their reported configuration. An idle service is not necessarily wasting meaningful resources."),
        ("4", "inspector", "Services · review before changing", "Read the selected service's role and consequences. Disabling a service can prevent dependent features from working. Undo controls restore supported changes; they are not a universal system backup."),
    ]
    .into_iter()
    .map(|(p, t, h, d)| S::new(p, t, h, d))
    .collect()
}
