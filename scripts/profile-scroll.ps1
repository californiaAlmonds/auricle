[CmdletBinding()]
param(
    [ValidateRange(1, 2147483647)]
    [int]$ProcessId,
    [ValidateRange(0, 32767)]
    [int]$ClientX = 650,
    [ValidateRange(0, 32767)]
    [int]$ClientY = 450,
    [ValidateRange(0.001, 86400)]
    [double]$Seconds = 12,
    [Alias('Interval')]
    [ValidateRange(1, 60000)]
    [int]$IntervalMs = 30,
    [ValidateRange(1, 2147483647)]
    [int]$SwitchEvery = 12,
    [ValidateRange(1, 32767)]
    [int]$WheelDelta = 120,
    [switch]$Horizontal,
    [switch]$Idle,
    [string]$LogPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not ('AuricleScrollBenchmarkNative' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.IO;
using System.Text;
using System.Text.RegularExpressions;
using System.Globalization;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Threading;

public static class AuricleScrollBenchmarkNative
{
    [StructLayout(LayoutKind.Sequential)]
    public struct Point { public int X; public int Y; }
    [StructLayout(LayoutKind.Sequential)]
    public struct Rect { public int Left; public int Top; public int Right; public int Bottom; }

    [DllImport("user32.dll")]
    private static extern bool SetForegroundWindow(IntPtr window);
    [DllImport("user32.dll")]
    private static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")]
    private static extern bool SetCursorPos(int screenX, int screenY);
    [DllImport("user32.dll")]
    private static extern bool ClientToScreen(IntPtr window, ref Point point);
    [DllImport("user32.dll")]
    private static extern bool GetClientRect(IntPtr window, out Rect rect);
    [DllImport("user32.dll")]
    private static extern bool IsWindowVisible(IntPtr window);
    [DllImport("user32.dll")]
    private static extern bool IsIconic(IntPtr window);
    [DllImport("user32.dll")]
    private static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);
    [DllImport("user32.dll", EntryPoint = "SendMessageTimeoutW", SetLastError = true)]
    private static extern IntPtr SendMessageTimeout(IntPtr window, uint message,
        UIntPtr wordParam, IntPtr longParam, uint flags, uint timeout, out UIntPtr result);

    public sealed class Result
    {
        public string mode;
        public int actions;
        public int messages;
        public int scheduledActions;
        public int skippedActions;
        public double requestedSeconds;
        public double seconds;
        public double cpuSeconds;
        public double cpuMachinePercent;
        public int logicalProcessors;
        public long workingSetStartBytes;
        public long workingSetEndBytes;
        public int intervalMs;
        public int switchEvery;
        public int wheelDelta;
        public int frameReports;
        public double? fpsMedian;
        public double? fpsMin;
        public double? fpsMax;
        public long? logStartIndex;
        public long? logEndIndex;
        public string logIndexUnit;
    }

    private sealed class DeadlineTimer : IDisposable
    {
        private readonly AutoResetEvent signal = new AutoResetEvent(false);
        private readonly Timer timer;
        private bool disposed;

        public DeadlineTimer()
        {
            timer = new Timer(state => {
                lock (signal) { if (!disposed) signal.Set(); }
            }, null, Timeout.Infinite, Timeout.Infinite);
        }

        public void WaitUntil(Stopwatch clock, double deadlineMs)
        {
            for (;;)
            {
                double remaining = deadlineMs - clock.Elapsed.TotalMilliseconds;
                if (remaining <= 0) return;
                timer.Change((int)Math.Ceiling(remaining), Timeout.Infinite);
                signal.WaitOne();
            }
        }

        public void Dispose()
        {
            lock (signal)
            {
                disposed = true;
                timer.Dispose();
                signal.Dispose();
            }
        }
    }

    private static IntPtr PackPoint(Point point)
    {
        return new IntPtr(unchecked((point.Y << 16) | (point.X & 0xffff)));
    }

    private static void CheckForeground(IntPtr window)
    {
        if (GetForegroundWindow() != window)
            throw new InvalidOperationException("Auricle is not foreground; benchmark aborted.");
    }

    private static void Send(IntPtr window, uint message, UIntPtr wordParam, IntPtr longParam)
    {
        CheckForeground(window);
        UIntPtr result;
        if (SendMessageTimeout(window, message, wordParam, longParam, 0x23, 250, out result) == IntPtr.Zero)
            throw new InvalidOperationException("Window message failed or stalled for 250 ms; benchmark aborted.");
    }

    private static bool AtLineBoundary(FileStream log, long offset, Encoding encoding)
    {
        if (offset == 0) return true;
        byte[] newline = encoding.GetBytes("\n");
        if (offset < newline.Length) return false;
        log.Position = offset - newline.Length;
        foreach (byte expected in newline)
            if (log.ReadByte() != expected) return false;
        return true;
    }

    private static void ReadFps(FileStream log, Encoding encoding, Result result)
    {
        long start = result.logStartIndex.Value;
        long end = result.logEndIndex.Value;
        if (end < start || log.Length < end)
            throw new InvalidOperationException("Performance log was truncated during the benchmark.");
        long length = end - start;
        if (length > 64 * 1024 * 1024)
            throw new InvalidOperationException("Performance log interval exceeds the 64 MiB read limit.");
        bool completeStart = AtLineBoundary(log, start, encoding);
        log.Position = start;
        byte[] bytes = new byte[(int)length];
        int received = 0;
        while (received < bytes.Length)
        {
            int count = log.Read(bytes, received, bytes.Length - received);
            if (count == 0)
                throw new InvalidOperationException("Performance log changed while reading the measured interval.");
            received += count;
        }
        string[] lines = encoding.GetString(bytes).Split('\n');
        var reports = new List<double>();
        var pattern = new Regex(@"average frames per second:\s*(?<fps>\d+(?:\.\d+)?)|(?<![\w.])(?<fps>\d+(?:\.\d+)?)\s+fps\b|\bfps\s*[:=]\s*(?<fps>\d+(?:\.\d+)?)\b", RegexOptions.IgnoreCase);
        for (int index = completeStart ? 0 : 1; index < lines.Length - 1; index++)
        {
            Match match = pattern.Match(lines[index]);
            double value;
            if (match.Success && Double.TryParse(match.Groups["fps"].Value,
                NumberStyles.AllowDecimalPoint, CultureInfo.InvariantCulture, out value))
                reports.Add(value);
        }
        reports.Sort();
        result.frameReports = reports.Count;
        if (reports.Count == 0) return;
        result.fpsMin = reports[0];
        result.fpsMax = reports[reports.Count - 1];
        int middle = reports.Count / 2;
        result.fpsMedian = reports.Count % 2 == 0
            ? (reports[middle - 1] + reports[middle]) / 2 : reports[middle];
    }

    public static Result Run(int processId, int clientX, int clientY, double seconds,
        int intervalMs, int switchEvery, int wheelDelta, bool horizontal, bool idle, FileStream log)
    {
        using (Process process = Process.GetProcessById(processId))
        using (var timer = new DeadlineTimer())
        {
            if (process.ProcessName != "auricle" && process.ProcessName != "native_shell")
                throw new InvalidOperationException("The selected process is not an Auricle native app.");
            IntPtr window = IntPtr.Zero;
            Point client = new Point { X = clientX, Y = clientY };
            Point screen = client;
            if (!idle)
            {
                window = process.MainWindowHandle;
                uint owner;
                Rect bounds;
                if (window == IntPtr.Zero || GetWindowThreadProcessId(window, out owner) == 0 ||
                    owner != processId || !IsWindowVisible(window) || IsIconic(window))
                    throw new InvalidOperationException("Auricle must already have a visible, non-minimized window.");
                if (!GetClientRect(window, out bounds) || clientX >= bounds.Right || clientY >= bounds.Bottom)
                    throw new InvalidOperationException("The requested point is outside the client area.");
                if (!ClientToScreen(window, ref screen) || screen.X < -32768 || screen.X > 32767 ||
                    screen.Y < -32768 || screen.Y > 32767)
                    throw new InvalidOperationException("The screen point cannot be represented in a wheel message.");
                SetForegroundWindow(window);
                CheckForeground(window);
                if (!SetCursorPos(screen.X, screen.Y))
                    throw new InvalidOperationException("Could not move the cursor to the client point.");
                CheckForeground(window);
            }

            Encoding encoding = Encoding.UTF8;
            if (log != null)
            {
                log.Position = 0;
                using (var reader = new StreamReader(log, Encoding.UTF8, true, 1024, true))
                {
                    reader.Peek();
                    encoding = reader.CurrentEncoding;
                }
            }
            double durationMs = seconds * 1000;
            var result = new Result {
                mode = idle ? "idle" : horizontal ? "horizontal" : "vertical",
                requestedSeconds = seconds,
                intervalMs = intervalMs,
                switchEvery = switchEvery,
                wheelDelta = wheelDelta,
                logicalProcessors = Environment.ProcessorCount,
                scheduledActions = idle ? 0 : (int)Math.Ceiling(durationMs / intervalMs)
            };
            var clock = new Stopwatch();
            process.Refresh();
            result.workingSetStartBytes = process.WorkingSet64;
            double cpuStart = process.TotalProcessorTime.TotalSeconds;
            if (log != null)
            {
                result.logIndexUnit = "bytes";
                result.logStartIndex = log.Length;
            }
            clock.Start();
            int slot = 0;
            while (!idle && slot < result.scheduledActions)
            {
                timer.WaitUntil(clock, slot * (double)intervalMs);
                double elapsed = clock.Elapsed.TotalMilliseconds;
                if (elapsed >= durationMs) break;
                slot = Math.Max(slot, (int)Math.Floor(elapsed / intervalMs));
                int direction = (slot / switchEvery) % 2 == 0 ? -1 : 1;
                int delta = direction * wheelDelta * (horizontal ? -1 : 1);
                Send(window, 0x0200, UIntPtr.Zero, PackPoint(client));
                result.messages++;
                if (clock.Elapsed.TotalMilliseconds >= durationMs) break;
                Send(window, horizontal ? 0x020eU : 0x020aU,
                    new UIntPtr(unchecked((uint)(delta << 16))), PackPoint(screen));
                result.messages++;
                result.actions++;
                slot++;
            }
            timer.WaitUntil(clock, durationMs);
            clock.Stop();
            if (log != null) result.logEndIndex = log.Length;
            process.Refresh();
            result.cpuSeconds = process.TotalProcessorTime.TotalSeconds - cpuStart;
            result.workingSetEndBytes = process.WorkingSet64;
            result.seconds = clock.Elapsed.TotalSeconds;
            result.cpuMachinePercent = 100 * result.cpuSeconds / result.seconds / result.logicalProcessors;
            result.skippedActions = result.scheduledActions - result.actions;
            if (log != null) ReadFps(log, encoding, result);
            return result;
        }
    }
}
'@
}

$logStream = $null
try {
    if (-not $PSBoundParameters.ContainsKey('ProcessId')) {
        $candidates = @(Get-Process -Name auricle -ErrorAction SilentlyContinue)
        if ($candidates.Count -ne 1) {
            throw [InvalidOperationException]::new('Expected exactly one running auricle process; specify -ProcessId.')
        }
        $ProcessId = $candidates[0].Id
    }
    if ($LogPath) {
        $resolvedLog = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($LogPath)
        $logStream = [IO.FileStream]::new($resolvedLog, [IO.FileMode]::Open, [IO.FileAccess]::Read,
            ([IO.FileShare]::ReadWrite -bor [IO.FileShare]::Delete))
    }
    $result = [AuricleScrollBenchmarkNative]::Run($ProcessId, $ClientX, $ClientY, $Seconds,
        $IntervalMs, $SwitchEvery, $WheelDelta, $Horizontal.IsPresent, $Idle.IsPresent, $logStream)
    $result | ConvertTo-Json
}
catch {
    $failure = $_.Exception.GetBaseException()
    if ($failure -is [InvalidOperationException]) {
        throw $failure.Message
    }
    throw 'Benchmark failed; verify process availability and optional log readability. No raw process or log data is reported.'
}
finally {
    if ($null -ne $logStream) { $logStream.Dispose() }
}