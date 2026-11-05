using System;
using System.Runtime.InteropServices;
using System.Threading;

public static class IgnoringConsole
{
    public delegate bool HandlerRoutine(uint control);
    private static readonly ManualResetEvent Stopped = new ManualResetEvent(false);
    private static readonly HandlerRoutine Stop = delegate(uint control)
    {
        if (control != 1) return false;
        Stopped.Set();
        return true;
    };

    [DllImport("kernel32.dll", EntryPoint = "SetConsoleCtrlHandler", SetLastError = true)]
    private static extern bool Ignore(IntPtr handler, bool add);

    [DllImport("kernel32.dll", EntryPoint = "SetConsoleCtrlHandler", SetLastError = true)]
    private static extern bool Register(HandlerRoutine handler, bool add);

    public static void Main()
    {
        if (!Ignore(IntPtr.Zero, true))
            throw new InvalidOperationException("Could not ignore CTRL_C: " + Marshal.GetLastWin32Error());
        if (!Register(Stop, true))
            throw new InvalidOperationException("Could not register CTRL_BREAK handler: " + Marshal.GetLastWin32Error());
        Console.Error.WriteLine("console.fixture.ready");
        Stopped.WaitOne();
    }
}
