# whouse

A terminal monitor for Linux and Windows with two separate views:

```sh
# Processes holding a file or folder open
whouse /path/to/watch
whouse 'C:\Users\Regan\Downloads'

# Read I/O history across all accessible processes
whouse --mode read-history

# Watch a specific process, with one sample per second
whouse --mode read-history --pid 1234 --interval 1000
```

Build with `cargo build --release`. Run in an interactive terminal.

Press `m` to switch between path monitoring and process read history. Use arrow
keys or the mouse to select a process, `/` to filter, `s` to change sorting,
`i` to hide idle rows, `p` to pause, `+`/`-` to change the sampling interval,
and `?` for help. Press `q` to quit. A PID filter applies only to history mode.

Read history stores up to 240 samples per process, showing a selected process's
read throughput, peak, observed byte total, and recent timestamped samples.
It also retains up to 256 recently observed files or handles per process, with
full paths, read/write access, file type, open/closed/unseen status, first and last
observation times, file position/size, and estimated read/write activity. Press
Tab to select and scroll the file history. Closed file paths remain searchable.
Press Enter on a file to inspect its full path, timestamps, size, offset, and
last observed read/write activity.
The first sample establishes a baseline; totals count bytes observed after
that baseline, rather than the process's lifetime total. Vanished or inaccessible
processes are marked `unseen` and their recent history is kept for two minutes.
Reused PIDs get fresh histories. Switching modes starts a fresh monitoring session.
History is held in memory for the current session.

Linux history uses `/proc/<pid>/io`: `rchar` counts read syscall bytes, including
cached reads, and `read_bytes` provides separate storage read rates. Windows uses
`GetProcessIoCounters` read-transfer bytes, which cover file, network, and device
I/O; a separate physical-disk read counter is unavailable and displays `n/a`.
History monitoring does not depend on finding files open under a watched path.
Files are sampled across all paths: Linux inspects process descriptors (including
directories, pipes and sockets), and Windows inspects disk file handles. A handle
being open does not prove that bytes were read. File rates come from offset
changes and are estimates; short-lived accesses between samples can be missed.

Path rates estimate movement of file offsets, so seeks can inflate them, and
positional reads, memory-mapped reads, or files opened and closed between samples
can be missed. Windows path monitoring inspects disk file handles using native
handle enumeration; it does not enumerate memory mappings or working directories.
Shared handles are marked, and aggregate path totals avoid double counting them.
The Windows handle enumeration interface is version dependent; query failures
appear in the target panel. `--no-maps` applies to Linux path monitoring only.

Run as root on Linux or Administrator on Windows to inspect more processes.
Protected processes may remain inaccessible. Windows shows executable paths
instead of full command lines and does not currently resolve process owners.
