using System.Runtime.InteropServices.WindowsRuntime;
using System.Text.Json;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Media.Imaging;
using MediaLake.Native;
using Windows.Graphics.Imaging;
using Windows.Storage.Streams;

namespace MediaLake;

public partial class App : Application
{
    internal static CoreClient? Core { get; private set; }
    public static string? StartupError { get; private set; }

    private Mutex? _mutex;
    private MainWindow? _main;
    private SettingsWindow? _settings;
    private TrayIcon? _tray;

    public App()
    {
        InitializeComponent();
        UnhandledException += (_, e) =>
        {
            LogLine("unhandled: " + e.Exception);
            e.Handled = true;
        };
    }

    public static void LogLine(string s)
    {
        try
        {
            var dir = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "MediaLake", "logs");
            Directory.CreateDirectory(dir);
            File.AppendAllText(Path.Combine(dir, "ui.log"), $"{DateTime.Now:O} {s}{Environment.NewLine}");
        }
        catch { }
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        var argv = Environment.GetCommandLineArgs();
        var selfTest = argv.Contains("--selftest");
        var shotIdx = Array.IndexOf(argv, "--screenshot");
        var shot = shotIdx >= 0 && shotIdx + 1 < argv.Length ? argv[shotIdx + 1] : null;
        var testMode = selfTest || shot != null;

        // single instance: a second launch just asks the running one to show itself
        _mutex = new Mutex(true, testMode ? "MediaLake.Test." + Environment.ProcessId : "MediaLake.Instance", out var first);
        var showEvent = new EventWaitHandle(false, EventResetMode.AutoReset, "MediaLake.Show");
        if (!first)
        {
            showEvent.Set();
            Exit();
            return;
        }

        try
        {
            var opts = new { server_exe = Path.Combine(AppContext.BaseDirectory, "medialake-server.exe") };
            Core = CoreClient.Create(Environment.GetEnvironmentVariable("MEDIALAKE_HOME"), opts);
        }
        catch (Exception ex)
        {
            StartupError = ex.Message;
            LogLine("core init failed: " + ex);
        }

        _main = new MainWindow();
        var dq = _main.DispatcherQueue;
        if (Core != null)
        {
            Core.EventReceived += ev => dq.TryEnqueue(() =>
            {
                _main?.OnCoreEvent(ev);
                _settings?.OnCoreEvent(ev);
            });
        }

        // the window must be activated once so its content loads; keep it out of sight
        _main.AppWindow.Move(new Windows.Graphics.PointInt32(-32000, -32000));
        _main.Activate();
        _main.Hide();

        Hotkey.Attach(_main.Hwnd);
        Hotkey.Pressed += () => dq.TryEnqueue(Toggle);
        var hotkey = "Ctrl+Alt+Space";
        try
        {
            if (Core != null) hotkey = Core.Call("get_settings").GetProperty("hotkey").GetString() ?? hotkey;
        }
        catch { }
        var err = Hotkey.Register(hotkey);
        if (err != null) LogLine($"hotkey {hotkey}: {err}");

        _tray = new TrayIcon(_main.Hwnd, "MediaLake  (" + hotkey + ")");
        _tray.Open += () => dq.TryEnqueue(Toggle);
        _tray.Settings += () => dq.TryEnqueue(ShowSettings);
        _tray.Quit += () => dq.TryEnqueue(Quit);

        var waiter = new Thread(() =>
        {
            while (true)
            {
                showEvent.WaitOne();
                dq.TryEnqueue(() => _main?.ShowSearch());
            }
        }) { IsBackground = true };
        waiter.Start();

        if (StartupError != null) _main.OnCoreEvent(JsonDocument.Parse($"{{\"type\":\"error\",\"message\":{JsonSerializer.Serialize(StartupError)}}}").RootElement);

        if (testMode) _ = RunTestAsync(selfTest, shot);
        else if (!argv.Contains("--background") && Core != null && IsFirstRun()) _main.ShowSearch();
    }

    private static bool IsFirstRun()
    {
        try { return Core!.Call("get_settings").GetProperty("dirs").GetArrayLength() == 0; }
        catch { return false; }
    }

    private void Toggle()
    {
        if (_main == null) return;
        if (_main.IsShown) _main.Hide(); else _main.ShowSearch();
    }

    public void ShowSettings()
    {
        if (_settings == null)
        {
            _settings = new SettingsWindow();
            _settings.Closed += (_, _) => _settings = null;
        }
        _settings.Activate();
    }

    public void ApplyHotkey(string hotkey) => Hotkey.Register(hotkey);

    public void QuitApp() => Quit();

    private void Quit()
    {
        _tray?.Dispose();
        Hotkey.Unregister();
        Core?.Dispose();
        Environment.Exit(0);
    }

    // ---------- automated checks (--selftest / --screenshot) ----------

    private async Task RunTestAsync(bool selfTest, string? shotPath)
    {
        var log = new List<string>();
        var main = _main!;
        main.KeepOpen = true;
        try
        {
            main.ShowSearch();
            await Task.Delay(800);
            var query = Environment.GetEnvironmentVariable("MEDIALAKE_TEST_QUERY") ?? "a cat";
            // wait for the first index run so there is something to find
            var deadline = DateTime.UtcNow.AddSeconds(Environment.GetEnvironmentVariable("MEDIALAKE_TEST_NOWAIT") != null ? 0 : 240);
            while (DateTime.UtcNow < deadline)
            {
                var st = await Core!.CallAsync("status");
                if (!st.GetProperty("busy").GetBoolean() && st.GetProperty("files").GetUInt64() > 0) break;
                await Task.Delay(500);
            }
            main.SetQuery(query);
            await Task.Delay(Environment.GetEnvironmentVariable("MEDIALAKE_TEST_WAIT") is { } w ? int.Parse(w) : 6000);
            log.Add($"items={main.Items.Count}");
            foreach (var it in main.Items) log.Add($"  {it.ScoreText} {it.Modality} {it.Path}");
            log.Add("status=" + Core!.Call("status"));
            if (shotPath != null) await SaveScreenshotAsync(main.RootElement, shotPath);
            if (Environment.GetEnvironmentVariable("MEDIALAKE_TEST_SETTINGS") is { } pagePath)
            {
                var parts = pagePath.Split('|');
                ShowSettings();
                await Task.Delay(1500);
                _settings!.SelectPage(int.Parse(parts[0]));
                await Task.Delay(1500);
                await SaveScreenshotAsync(_settings.RootElement, parts[1]);
            }
        }
        catch (Exception ex) { log.Add("FAIL " + ex); }
        File.WriteAllLines(Path.Combine(AppContext.BaseDirectory, "selftest.log"), log);
        Quit();
    }

    private static async Task SaveScreenshotAsync(FrameworkElement el, string path)
    {
        if (el is Microsoft.UI.Xaml.Controls.Grid g && Application.Current.Resources.TryGetValue("SolidBackgroundFillColorBaseBrush", out var bg)) g.Background = (Microsoft.UI.Xaml.Media.Brush)bg;
        else if (el is Microsoft.UI.Xaml.Controls.NavigationView nv && Application.Current.Resources.TryGetValue("SolidBackgroundFillColorBaseBrush", out var bg2)) nv.Background = (Microsoft.UI.Xaml.Media.Brush)bg2;
        await Task.Delay(300);
        var rtb = new RenderTargetBitmap();
        await rtb.RenderAsync(el);
        var pixels = await rtb.GetPixelsAsync();
        using var stream = new InMemoryRandomAccessStream();
        var enc = await BitmapEncoder.CreateAsync(BitmapEncoder.PngEncoderId, stream);
        enc.SetPixelData(BitmapPixelFormat.Bgra8, BitmapAlphaMode.Premultiplied, (uint)rtb.PixelWidth, (uint)rtb.PixelHeight, 96, 96, pixels.ToArray());
        await enc.FlushAsync();
        stream.Seek(0);
        var bytes = new byte[stream.Size];
        await stream.ReadAsync(bytes.AsBuffer(), (uint)stream.Size, InputStreamOptions.None);
        await File.WriteAllBytesAsync(path, bytes);
    }
}
