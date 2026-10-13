using System.Diagnostics;
using System.Runtime.InteropServices;

namespace MediaLake.Native;

/// <summary>Window message hook (hotkey + tray) installed on one HWND via a comctl32 subclass.</summary>
internal static unsafe class MessageHook
{
    private static readonly List<Func<nint, uint, nint, nint, bool>> Handlers = [];
    private static bool _installed;

    public static void Install(nint hwnd, Func<nint, uint, nint, nint, bool> handler)
    {
        Handlers.Add(handler);
        if (_installed) return;
        _installed = Win32.SetWindowSubclass(hwnd, &Proc, 1, 0);
    }

    [UnmanagedCallersOnly(CallConvs = [typeof(System.Runtime.CompilerServices.CallConvStdcall)])]
    private static nint Proc(nint hwnd, uint msg, nint w, nint l, nuint id, nuint data)
    {
        try
        {
            foreach (var h in Handlers)
                if (h(hwnd, msg, w, l)) return 0;
        }
        catch { }
        return Win32.DefSubclassProc(hwnd, msg, w, l);
    }
}

internal static class Hotkey
{
    private const int Id = 0x5E4C;
    private static nint _hwnd;

    public static event Action? Pressed;

    /// <summary>Parses "Ctrl+Alt+Space" etc. Returns false for invalid combinations.</summary>
    public static bool TryParse(string text, out uint mods, out uint vk)
    {
        mods = 0; vk = 0;
        foreach (var raw in text.Split('+', StringSplitOptions.TrimEntries | StringSplitOptions.RemoveEmptyEntries))
        {
            switch (raw.ToLowerInvariant())
            {
                case "ctrl" or "control": mods |= Win32.MOD_CONTROL; continue;
                case "alt": mods |= Win32.MOD_ALT; continue;
                case "shift": mods |= Win32.MOD_SHIFT; continue;
                case "win" or "meta": mods |= Win32.MOD_WIN; continue;
            }
            if (vk != 0) return false;
            var name = raw.Length == 1 && char.IsDigit(raw[0]) ? "Number" + raw : raw;
            if (!Enum.TryParse<Windows.System.VirtualKey>(name, true, out var k) || k == Windows.System.VirtualKey.None) return false;
            vk = (uint)k;
        }
        return vk != 0 && mods != 0;
    }

    public static void Attach(nint hwnd)
    {
        _hwnd = hwnd;
        MessageHook.Install(hwnd, (_, msg, w, _) =>
        {
            if (msg == Win32.WM_HOTKEY && w == Id) { Pressed?.Invoke(); return true; }
            return false;
        });
    }

    /// <summary>Registers the hotkey; the previous one is released first. Returns an error text or null.</summary>
    public static string? Register(string text)
    {
        // no shortcut is a valid choice: release the old one
        if (string.IsNullOrWhiteSpace(text))
        {
            Win32.UnregisterHotKey(_hwnd, Id);
            return null;
        }
        if (!TryParse(text, out var mods, out var vk)) return "Use at least one modifier and one key, e.g. Ctrl+Alt+Space.";
        Win32.UnregisterHotKey(_hwnd, Id);
        if (!Win32.RegisterHotKey(_hwnd, Id, mods | Win32.MOD_NOREPEAT, vk))
            return "This shortcut is already used by another application.";
        return null;
    }

    public static void Unregister() => Win32.UnregisterHotKey(_hwnd, Id);
}

internal sealed class TrayIcon : IDisposable
{
    private const uint NIM_ADD = 0, NIM_MODIFY = 1, NIM_DELETE = 2;
    private const uint NIF_MESSAGE = 1, NIF_ICON = 2, NIF_TIP = 4, NIF_INFO = 0x10;
    private const int NIN_BALLOONUSERCLICK = 0x405;
    private const int WM_TRAY = Win32.WM_APP + 1;
    private readonly nint _hwnd;
    private NOTIFYICONDATA _data;

    public event Action? Open;
    public event Action? Settings;
    public event Action? Quit;
    /// <summary>"Pause all syncing" (true) / "Resume all syncing" (false) from the menu.</summary>
    public event Action<bool>? PauseAll;
    /// <summary>The user clicked a balloon notification.</summary>
    public event Action? BalloonClicked;

    private string[] _syncLines = [];
    private bool _allPaused;

    public TrayIcon(nint hwnd, string tooltip)
    {
        _hwnd = hwnd;
        var exe = Process.GetCurrentProcess().MainModule?.FileName ?? "";
        _data = new NOTIFYICONDATA
        {
            cbSize = Marshal.SizeOf<NOTIFYICONDATA>(),
            hWnd = hwnd,
            uID = 1,
            uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP,
            uCallbackMessage = WM_TRAY,
            hIcon = Win32.ExtractIconW(0, exe, 0),
            szTip = tooltip,
            szInfo = "",
            szInfoTitle = "",
        };
        Win32.Shell_NotifyIconW(NIM_ADD, ref _data);
        MessageHook.Install(hwnd, OnMessage);
    }

    private bool OnMessage(nint hwnd, uint msg, nint w, nint l)
    {
        if (msg != WM_TRAY) return false;
        var ev = (int)(l & 0xFFFF);
        if (ev == Win32.WM_LBUTTONUP) Open?.Invoke();
        else if (ev == Win32.WM_RBUTTONUP) ShowMenu();
        else if (ev == NIN_BALLOONUSERCLICK) BalloonClicked?.Invoke();
        return true;
    }

    private void ShowMenu()
    {
        Win32.GetCursorPos(out var p);
        var menu = Win32.CreatePopupMenu();
        Win32.AppendMenuW(menu, 0, 1, "Open MediaLake");
        Win32.AppendMenuW(menu, 0, 2, "Settings");
        if (_syncLines.Length > 0)
        {
            Win32.AppendMenuW(menu, 0x800, 0, null); // separator
            foreach (var l in _syncLines) Win32.AppendMenuW(menu, 0x3 /* MF_GRAYED | MF_DISABLED */, 0, l);
            Win32.AppendMenuW(menu, 0, 4, _allPaused ? "Resume all syncing" : "Pause all syncing");
        }
        Win32.AppendMenuW(menu, 0x800, 0, null); // separator
        Win32.AppendMenuW(menu, 0, 3, "Quit");
        Win32.SetForegroundWindow(_hwnd); // required for the menu to dismiss properly
        var cmd = Win32.TrackPopupMenuEx(menu, 0x100 /*TPM_RETURNCMD*/ | 0x20 /*TPM_RIGHTBUTTON*/, p.X, p.Y, _hwnd, 0);
        Win32.DestroyMenu(menu);
        switch (cmd)
        {
            case 1: Open?.Invoke(); break;
            case 2: Settings?.Invoke(); break;
            case 3: Quit?.Invoke(); break;
            case 4: PauseAll?.Invoke(!_allPaused); break;
        }
    }

    /// <summary>The sync lines of the menu and the tooltip (what the icon says when hovered).</summary>
    public void SetSync(string[] lines, bool allPaused, string tooltip)
    {
        _syncLines = lines;
        _allPaused = allPaused;
        _data.szTip = tooltip.Length > 120 ? tooltip[..120] : tooltip;
        _data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        Win32.Shell_NotifyIconW(NIM_MODIFY, ref _data);
    }

    /// <summary>A balloon notification from the icon (no registration needed; a click raises <see cref="BalloonClicked"/>).</summary>
    public void Balloon(string title, string text)
    {
        _data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_INFO;
        _data.szInfoTitle = title.Length > 60 ? title[..60] : title;
        _data.szInfo = text.Length > 250 ? text[..250] : text;
        _data.dwInfoFlags = 1; // NIIF_INFO
        Win32.Shell_NotifyIconW(NIM_MODIFY, ref _data);
        _data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        _data.szInfo = "";
        _data.szInfoTitle = "";
    }

    public void Dispose() => Win32.Shell_NotifyIconW(NIM_DELETE, ref _data);
}

internal static class ShellActions
{
    public static void OpenFile(string path) => Win32.ShellExecuteW(0, "open", path, null, null, 1);

    public static void ShowInExplorer(string path) =>
        Process.Start(new ProcessStartInfo("explorer.exe", $"/select,\"{path}\"") { UseShellExecute = false });
}
