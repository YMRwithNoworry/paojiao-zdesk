using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading.Tasks;

namespace Listary.FileAppPlugin.ZSDesk
{
    public sealed class ZSDeskPlugin : IFileAppPlugin
    {
        private IFileAppPluginHost host;
        public bool IsOpenedFolderProvider => true;
        public bool IsQuickSwitchTarget => false;
        public bool IsSharedAcrossApplications => false;
        public SearchBarType SearchBarType => SearchBarType.Floating;

        public Task<bool> Initialize(IFileAppPluginHost value)
        {
            host = value ?? throw new ArgumentNullException(nameof(value));
            return Task.FromResult(true);
        }

        public IFileWindow BindFileWindow(IntPtr window)
        {
            if (host == null || Native.ClassName(window) != "ZSDesk.Native.Explorer") return null;
            var endpoint = Native.Endpoint(window);
            if (endpoint == IntPtr.Zero) return null;
            uint process;
            Native.GetWindowThreadProcessId(window, out process);
            return process == 0 || process == Native.GetCurrentProcessId() ? null : new ZSDeskWindow(window, process);
        }
    }

    public sealed class ZSDeskWindow : IFileWindow
    {
        private readonly uint process;
        public IntPtr Handle { get; }
        internal ZSDeskWindow(IntPtr window, uint process)
        {
            this.process = process;
            Handle = window;
        }
        public Task<IFileTab> GetCurrentTab() => Task.FromResult<IFileTab>(new ZSDeskTab(Handle, process));
    }

    public sealed class ZSDeskTab : IFileTab, IGetFolder
    {
        private readonly IntPtr window;
        private readonly uint process;
        internal ZSDeskTab(IntPtr window, uint process)
        {
            this.window = window;
            this.process = process;
        }
        public Task<string> GetCurrentFolder()
        {
            uint currentProcess;
            Native.GetWindowThreadProcessId(window, out currentProcess);
            if (currentProcess != process || currentProcess == Native.GetCurrentProcessId()
                || Native.ClassName(window) != "ZSDesk.Native.Explorer") return Task.FromResult(string.Empty);
            var endpoint = Native.Endpoint(window);
            if (endpoint == IntPtr.Zero) return Task.FromResult(string.Empty);
            // This custom endpoint uses DefWindowProc's system-maintained text.
            // Cross-process GetWindowText reads that caption without sending a
            // synchronous message to ZSdesk's UI thread. The foreign-process
            // check above deliberately excludes GetWindowText's same-process path.
            const int characters = 32768;
            var buffer = new StringBuilder(characters);
            int copied = Native.GetWindowTextW(endpoint, buffer, characters);
            if (copied <= 0 || copied >= characters) return Task.FromResult(string.Empty);
            Native.GetWindowThreadProcessId(window, out currentProcess);
            if (currentProcess != process || Native.Endpoint(window) != endpoint) return Task.FromResult(string.Empty);
            string folder = buffer.ToString();
            return Task.FromResult(Native.AbsoluteFolder(folder) ? folder : string.Empty);
        }
    }

    internal static class Native
    {
        [DllImport("user32.dll", CharSet = CharSet.Unicode)]
        private static extern int GetClassNameW(IntPtr window, StringBuilder value, int capacity);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)]
        internal static extern int GetWindowTextW(IntPtr window, StringBuilder value, int capacity);
        [DllImport("user32.dll", CharSet = CharSet.Unicode)]
        private static extern IntPtr FindWindowExW(IntPtr parent, IntPtr after, string className, string caption);
        [DllImport("user32.dll")]
        internal static extern uint GetWindowThreadProcessId(IntPtr window, out uint process);
        [DllImport("kernel32.dll")]
        internal static extern uint GetCurrentProcessId();
        internal static string ClassName(IntPtr window)
        {
            var value = new StringBuilder(128);
            return GetClassNameW(window, value, value.Capacity) == 0 ? string.Empty : value.ToString();
        }
        internal static IntPtr Endpoint(IntPtr parent)
        {
            var endpoint = FindWindowExW(parent, IntPtr.Zero, "ZSDesk.Listary.Folder.v1", null);
            uint parentProcess, childProcess;
            GetWindowThreadProcessId(parent, out parentProcess);
            GetWindowThreadProcessId(endpoint, out childProcess);
            return parentProcess != 0 && parentProcess == childProcess ? endpoint : IntPtr.Zero;
        }
        internal static bool AbsoluteFolder(string value)
        {
            if (string.IsNullOrEmpty(value) || value.IndexOf('\0') >= 0) return false;
            return (value.Length >= 3 && char.IsLetter(value[0]) && value[1] == ':' && (value[2] == '\\' || value[2] == '/'))
                || (value.StartsWith(@"\\", StringComparison.Ordinal) && value.Length > 2);
        }
    }
}
