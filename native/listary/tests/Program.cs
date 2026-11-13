using System;
using System.Runtime.InteropServices;
using System.Threading;
using Listary.FileAppPlugin;
using Listary.FileAppPlugin.ZSDesk;
using Microsoft.Extensions.Logging;
using Microsoft.Extensions.Logging.Abstractions;

internal static class Program
{
    private static int Main(string[] args)
    {
        IntPtr desktop = IntPtr.Zero;
        IntPtr original = GetThreadDesktop(GetCurrentThreadId());
        try
        {
            string desktopName = Environment.GetEnvironmentVariable("ZSDESK_LISTARY_TEST_DESKTOP");
            if (!string.IsNullOrEmpty(desktopName))
            {
                desktop = OpenDesktopW(desktopName, 0, false, 0x0081);
                if (desktop == IntPtr.Zero || !SetThreadDesktop(desktop)) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
            }
            if (args.Length != 2) return 2;
            var window = new IntPtr(unchecked((long)ulong.Parse(args[0])));
            var plugin = new ZSDeskPlugin();
            if (!plugin.Initialize(new Host()).GetAwaiter().GetResult()) return 3;
            if (!plugin.IsOpenedFolderProvider || plugin.IsQuickSwitchTarget || plugin.IsSharedAcrossApplications) return 4;
            if (plugin.BindFileWindow(IntPtr.Zero) != null) return 5;
            WaitForPausedTarget();
            var bound = plugin.BindFileWindow(window);
            if (bound == null || bound.Handle != window) return 6;
            var tab = bound.GetCurrentTab().GetAwaiter().GetResult() as IGetFolder;
            if (tab == null) return 7;
            string folder = tab.GetCurrentFolder().GetAwaiter().GetResult();
            if (!string.Equals(folder, args[1], StringComparison.Ordinal)) return 8;
            Console.WriteLine("Listary IGetFolder contract passed.");
            return 0;
        }
        catch (Exception error)
        {
            Console.Error.WriteLine(error);
            return 1;
        }
        finally
        {
            if (desktop != IntPtr.Zero) { SetThreadDesktop(original); CloseDesktop(desktop); }
        }
    }
    [DllImport("user32.dll", CharSet=CharSet.Unicode, SetLastError=true)] private static extern IntPtr OpenDesktopW(string name, uint flags, bool inherit, uint access);
    [DllImport("user32.dll", SetLastError=true)] private static extern bool SetThreadDesktop(IntPtr desktop);
    [DllImport("user32.dll")] private static extern IntPtr GetThreadDesktop(uint thread);
    [DllImport("user32.dll")] private static extern bool CloseDesktop(IntPtr desktop);
    [DllImport("kernel32.dll")] private static extern uint GetCurrentThreadId();
    private static void WaitForPausedTarget()
    {
        string ready = Environment.GetEnvironmentVariable("ZSDESK_LISTARY_TEST_READY");
        string run = Environment.GetEnvironmentVariable("ZSDESK_LISTARY_TEST_RUN");
        if (string.IsNullOrEmpty(ready) || string.IsNullOrEmpty(run)) return;
        using (var readyEvent = EventWaitHandle.OpenExisting(ready))
        using (var runEvent = EventWaitHandle.OpenExisting(run))
        {
            readyEvent.Set();
            if (!runEvent.WaitOne(10000)) throw new TimeoutException("Target did not begin the paused-pump check.");
        }
    }

    private sealed class Host : IFileAppPluginHost
    {
        public ILogger Logger => NullLogger.Instance;
        public IntPtr SendMessage(IntPtr window, uint message, IntPtr wp, IntPtr lp) => throw new NotSupportedException();
        public IntPtr SendMessage(IntPtr window, uint message, IntPtr wp, byte[] data) => throw new NotSupportedException("Folder reads must not message the target UI thread.");
        public IntPtr SendCopyData(IntPtr window, IntPtr source, IntPtr dataKind, byte[] data, uint size, uint timeout = 0) => throw new NotSupportedException();
        public bool PostMessage(IntPtr window, uint message, IntPtr wp, IntPtr lp) => throw new NotSupportedException();
    }
}
