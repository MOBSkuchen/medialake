using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;

namespace SearchNow.Native;

public sealed class CoreException(string message) : Exception(message);

/// <summary>Safe wrapper over the generated <see cref="NativeMethods"/> (JSON across the boundary).</summary>
internal sealed unsafe class CoreClient : IDisposable
{
    private static CoreClient? _instance;
    private Handle* _h;

    /// <summary>Raised on a Rust thread. Marshal to the UI thread yourself.</summary>
    public event Action<JsonElement>? EventReceived;

    private CoreClient(Handle* h) => _h = h;

    public static string Version() => Take(NativeMethods.sn_version());

    public static CoreClient Create(string? configDir, object? options)
    {
        var dir = Encoding.UTF8.GetBytes((configDir ?? "") + "\0");
        var opts = Encoding.UTF8.GetBytes(JsonSerializer.Serialize(options ?? new { }) + "\0");
        Handle* h;
        fixed (byte* d = dir)
        fixed (byte* o = opts)
            h = NativeMethods.sn_init(d, o);
        if (h == null) throw new CoreException(Take(NativeMethods.sn_last_error()));
        var c = new CoreClient(h);
        _instance = c;
        NativeMethods.sn_set_event_callback(h, &OnEvent);
        return c;
    }

    [UnmanagedCallersOnly(CallConvs = [typeof(System.Runtime.CompilerServices.CallConvCdecl)])]
    private static void OnEvent(byte* msg)
    {
        try
        {
            var s = Marshal.PtrToStringUTF8((nint)msg) ?? "";
            using var doc = JsonDocument.Parse(s);
            _instance?.EventReceived?.Invoke(doc.RootElement.Clone());
        }
        catch { /* never throw into native code */ }
    }

    /// <summary>Blocking call; runs on the thread pool.</summary>
    public Task<JsonElement> CallAsync(string method, object? args = null) => Task.Run(() => Call(method, args));

    public JsonElement Call(string method, object? args = null)
    {
        if (_h == null) throw new CoreException("core is shut down");
        var m = Encoding.UTF8.GetBytes(method + "\0");
        var a = Encoding.UTF8.GetBytes((args == null ? "" : JsonSerializer.Serialize(args)) + "\0");
        string json;
        fixed (byte* pm = m)
        fixed (byte* pa = a)
            json = Take(NativeMethods.sn_call(_h, pm, pa));
        using var doc = JsonDocument.Parse(json);
        var root = doc.RootElement;
        if (root.TryGetProperty("error", out var err)) throw new CoreException(err.GetString() ?? "error");
        return root.GetProperty("ok").Clone();
    }

    public void Dispose()
    {
        var h = _h;
        _h = null;
        if (h != null)
        {
            NativeMethods.sn_set_event_callback(h, null);
            NativeMethods.sn_shutdown(h);
        }
    }

    private static string Take(byte* p)
    {
        try { return Marshal.PtrToStringUTF8((nint)p) ?? ""; }
        finally { NativeMethods.sn_free_string(p); }
    }
}
