use toolboks_addon_sdk::TutorialStep as S;
pub(super) fn steps() -> Vec<S> {
    [
        ("0", "memory-summary", "Memory Cleaner · your memory", "In use is RAM currently occupied; Available is what Windows can offer applications. Cached memory is reusable, so a high used percentage alone does not mean anything is wrong."),
        ("0", "memory-optimize", "Trim idle apps", "This moves memory that idle apps are not using into the cache and lowers their priority for memory. It does not free memory, and apps may briefly read from disk again. The tour never presses this button."),
        ("0", "memory-commit", "Commit · promised memory", "Commit compares memory Windows has promised with its limit, which includes RAM and paging capacity. Approaching that limit is more useful evidence of pressure than a large cache."),
        ("0", "memory-apps", "Where your memory goes", "The breakdown groups application memory so you can see the largest users. Windows and other accounts for memory outside the named app groups. Read the last-run report after cleaning to assess the actual result."),
        ("0", "memory-options", "Automatic cleaning and exclusions", "Automatic cleaning is optional and off by default. The pressure rule only acts after memory has been under pressure for about a minute, at most once an hour. Two extra triggers, a memory limit and a timer, are also off by default and are set under Advanced, Rules. Nothing ever touches the app you are using. Games, game launchers and anti-cheat apps are never touched: Toolboks leaves them alone. Clearing the cache under Advanced options needs administrator permission. The tour leaves all these settings alone."),
        ("0", "memory-options", "Last run and history", "After a run, this area shows how much moved to cache, what came back within two minutes, and keeps this session's history. A momentary change does not necessarily improve performance."),
        ("1/selected", "memory-lists", "Advanced · memory lists", "The memory lists show how RAM is divided between active use, standby cache, modified pages and free memory. Standby memory is useful cached data that Windows can reclaim."),
        ("1/selected", "toolbar", "Find and group applications", "Search narrows the process table. Grouping combines processes with the same name. Sorting helps find large working sets or private allocations; it does not change the running applications."),
        ("1/selected", "table", "Select processes and lock exclusions", "The table compares working-set and private memory. Select rows to inspect or trim them, and lock an application to exclude it. Trimming moves pages out of the application's working set (they stay in RAM as cache); it does not close the application."),
        ("1/selected", "inspector", "Selected · process actions", "This inspector explains the selected processes and offers Trim selected. Protected or excluded processes can be skipped. Review the result rather than assuming every requested process was changed."),
        ("1/system", "inspector", "System · deeper operations", "The System tab exposes system memory-list operations. Some require administrator rights and can remove useful cache. The tour only opens the tab; it does not request permission or perform a clean."),
        ("1/rules", "inspector", "Rules · automatic behavior", "Rules controls automatic cleanup and lists excluded applications. These controls affect background behavior, so choose them deliberately. Opening this walkthrough does not turn any rule on."),
    ]
    .into_iter()
    .map(|(p, t, h, d)| S::new(p, t, h, d))
    .collect()
}
