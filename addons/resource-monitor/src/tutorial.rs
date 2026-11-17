use toolboks_addon_sdk::TutorialStep as S;
pub(super) fn steps() -> Vec<S> {
    [
        ("0", "resource-range", "Resource Monitor · time range", "Choose how much recorded history each graph shows. CPU rated power is an input for an estimate, not a measured processor wattage. The monitor keeps collecting while other tools are open."),
        ("0/cpu", "graph-cpu", "CPU · load and speed", "CPU usage shows how busy the processor is, and the graph shows how that changes over time. Speed and core counts provide context. A short spike is often normal; sustained load is more useful to investigate."),
        ("0/cpu", "top-processes", "Top processes · CPU", "This ranking identifies the largest current CPU users. It is a live snapshot rather than an all-time total. Multiple processes belonging to the same application may be grouped."),
        ("0/memory", "graph-memory", "Memory · RAM pressure", "Memory usage shows occupied physical RAM over time. Compare this with what you are running and the Memory Cleaner commit reading before treating high usage as a problem."),
        ("0/memory", "top-processes", "Top processes · Memory", "The Memory ranking highlights applications occupying the most RAM. It helps connect the system-wide memory graph to individual applications."),
        ("0/gpu", "graph-gpu", "GPU · graphics activity", "This graph shows activity reported by the graphics adapter. Availability depends on hardware and drivers; a missing reading is not proof that the GPU is idle or faulty."),
        ("0/gpu", "top-processes", "Top processes · GPU", "The GPU ranking shows process activity when the hardware exposes it. Missing per-process readings can be a reporting limitation."),
        ("0/disk", "graph-disk", "Disk · reads and writes", "Read and write rates show data moving to and from storage. Active time, when available, shows how busy the disk is. High activity can come from updates, copying files or paging."),
        ("0/disk", "top-processes", "Top processes · Disk", "Disk ranking highlights applications moving the most data. A high transfer rate can be expected during installs, downloads or file copies."),
        ("0/network", "graph-network", "Network · upload and download", "The network graph compares download and upload speed over time, and the list below shows which adapter is busiest. It shows traffic the system reports, not the speed your internet plan promises."),
        ("0/power", "graph-power", "Power · estimates and readings", "Power combines readings and estimates. The graphics card figure is measured when the driver supports it; the processor figure is an estimate from the rated power you enter. Neither is power at the wall."),
        ("1", "details-cpu", "Details · individual CPU cores", "Per-core histories help reveal work concentrated on a few threads. A program can be limited by one busy core even while the total CPU percentage looks modest."),
        ("1", "details-gpu", "Details · GPU engines", "Separate engines show 3D, copying, video decoding, encoding or compute work when supported. Different tasks stress different engines, so inspect the engine relevant to what you are doing."),
        ("1", "graph-power", "Details · power", "The same power history is available alongside the detailed hardware breakdowns. Treat estimates as context and compare trends over time."),
        ("1", "details-disk", "Details · each drive", "Per-drive graphs separate storage activity and capacity readings. This helps distinguish a busy system drive from another disk that is mostly idle."),
        ("1", "details-network", "Details · each network adapter", "Adapter histories separate Wi-Fi, Ethernet and other reported interfaces. Virtual adapters can appear too, so use the adapter name to understand the connection."),
        ("2", "toolbar", "Processes · search, views and pause", "Use the toolbar to filter by name or process ID, pick a view, or pause the displayed snapshot for inspection. By type sorts programs into Apps, Background processes and Windows processes; By name merges the processes of one program, and + shows them; Flat lists every process. Pausing the display does not suspend applications."),
        ("2", "table", "Processes · sort and select", "Sort columns to find large CPU or memory users and select a row to inspect it. A process ID identifies a particular running instance; grouped entries can represent several processes."),
        ("2", "inspector", "Processes · actions and protection", "The inspector provides information and available actions for selected processes. Ending a process can lose unsaved work; protection and confirmation checks apply. No process action runs during the tour."),
        ("3", "toolbar", "Connections · filters and snapshot", "Filter the connection list by application or address, and use quick filters to narrow the view. Pause freezes the displayed snapshot while applications continue running."),
        ("3", "table", "Connections · addresses and states", "Rows show protocol, local and remote endpoints, owning process and connection state. Listening sockets and active connections have different meanings; a connection alone does not establish whether an app is trustworthy."),
        ("3", "inspector", "Connections · inspect an endpoint", "Select a connection for process and address details. Copy actions put text on the clipboard. Resolve host name makes a network lookup only when you request it. Ending the owner process can interrupt the application."),
    ]
    .into_iter()
    .map(|(p, t, h, d)| S::new(p, t, h, d))
    .collect()
}
