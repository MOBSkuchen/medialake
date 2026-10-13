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
    private WelcomeWindow? _welcome;
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
        // the uninstaller's step: the synced folders go back to plain folders, then the app is gone
        if (argv.Contains("--unregister-sync-roots"))
        {
            UnregisterSyncRootsAndExit();
            return;
        }
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
                _welcome?.OnCoreEvent(ev);
                OnSyncEvent(ev);
            });
        }

        // the window must be activated once so its content loads; keep it out of sight
        _main.AppWindow.Move(new Windows.Graphics.PointInt32(-32000, -32000));
        _main.Activate();
        _main.Hide();

        Hotkey.Attach(_main.Hwnd);
        Hotkey.Pressed += () => dq.TryEnqueue(Toggle);
        // no shortcut is a valid choice (and what a fresh install has until the welcome window sets one)
        var hotkey = "";
        try
        {
            if (Core != null) hotkey = Core.Call("get_settings").GetProperty("hotkey").GetString() ?? "";
        }
        catch { }
        if (hotkey.Length > 0)
        {
            var err = Hotkey.Register(hotkey);
            if (err != null) LogLine($"hotkey {hotkey}: {err}");
        }

        _tray = new TrayIcon(_main.Hwnd, hotkey.Length > 0 ? "MediaLake  (" + hotkey + ")" : "MediaLake");
        _tray.Open += () => dq.TryEnqueue(Toggle);
        _tray.Settings += () => dq.TryEnqueue(ShowSettings);
        _tray.Quit += () => dq.TryEnqueue(Quit);
        _tray.PauseAll += paused => dq.TryEnqueue(async () =>
        {
            try { await Core!.CallAsync(paused ? "sync_pause" : "sync_resume"); } catch { }
            await RefreshSyncSummaryAsync();
        });
        _tray.BalloonClicked += () => dq.TryEnqueue(() =>
        {
            ShowSettings();
            _settings?.SelectSync();
        });
        _ = RefreshSyncSummaryAsync();

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
        else if (Core != null) _ = StartupAsync(argv.Contains("--background"));
    }

    /// <summary>
    /// A fresh install (no config yet) gets the welcome window, unless it was started in the background:
    /// that leaves it for the next time the app is opened. An app that has been set up opens nothing,
    /// except the search window when there is nothing to see yet.
    /// </summary>
    private async Task StartupAsync(bool background)
    {
        var welcomeDone = true;
        var ready = true;
        try
        {
            var cfg = await Core!.CallAsync("get_settings");
            welcomeDone = !cfg.TryGetProperty("welcome_done", out var w) || w.GetBoolean();
            var st = await Core.CallAsync("status");
            ready = !st.TryGetProperty("search", out var s) || s.GetString() == "ready";
        }
        catch { }
        await (_main?.RefreshSearchStateAsync() ?? Task.CompletedTask);
        if (!welcomeDone)
        {
            if (!background) ShowWelcome(0);
            return;
        }
        if (background) return;
        if (ready)
        {
            if (IsFirstRun()) _main?.ShowSearch();
        }
        else if (!await HasSyncedFoldersAsync())
        {
            _main?.ShowSearch(); // the search window explains what is missing
        }
        else
        {
            // MediaLake is used for syncing only: opening it shows the folders, not an empty search
            ShowSettings();
            _settings?.SelectSync();
        }
    }

    private static async Task<bool> HasSyncedFoldersAsync()
    {
        try
        {
            var st = await Core!.CallAsync("sync_status");
            return st.TryGetProperty("syncs", out var s) && s.ValueKind == JsonValueKind.Array && s.GetArrayLength() > 0;
        }
        catch { return false; }
    }

    // ---------- sync: tray lines and notifications ----------

    private readonly Dictionary<string, DateTime> _notified = new();
    private bool _summaryQueued;

    private void OnSyncEvent(JsonElement ev)
    {
        var type = ev.GetProperty("type").GetString() ?? "";
        if (!type.StartsWith("sync_")) return;
        string Field(string n) => ev.TryGetProperty(n, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() ?? "" : "";
        var name = Field("name") is { Length: > 0 } nm ? nm : "A synced folder";
        switch (type)
        {
            case "sync_conflict":
                Notify($"conflict-{name}-{Field("path")}", $"Conflict in {name}",
                    $"{System.IO.Path.GetFileName(Field("path"))} was changed here and on another computer. Choose which version to keep.");
                break;
            case "sync_state":
                switch (Field("state"))
                {
                    case "denied": Notify($"denied-{name}", $"{name} is no longer shared with you", "The computer that shares it withdrew access. Your downloaded files stay where they are."); break;
                    case "disk_full": Notify($"full-{name}", "Your disk is full", $"{name} cannot be synced until some space is freed."); break;
                    case "held": Notify($"held-{name}", $"Deletions in {name} are waiting", "Many files were removed here. Open Settings > Sync to confirm that they should be deleted everywhere."); break;
                }
                break;
        }
        if (_summaryQueued) return;
        _summaryQueued = true;
        _ = Task.Run(async () =>
        {
            await Task.Delay(500);
            _main?.DispatcherQueue.TryEnqueue(async () =>
            {
                _summaryQueued = false;
                await RefreshSyncSummaryAsync();
            });
        });
    }

    /// <summary>The same message at most every ten minutes.</summary>
    private void Notify(string id, string title, string text)
    {
        if (_notified.TryGetValue(id, out var t) && DateTime.UtcNow - t < TimeSpan.FromMinutes(10)) return;
        _notified[id] = DateTime.UtcNow;
        _tray?.Balloon(title, text);
    }

    private async Task RefreshSyncSummaryAsync()
    {
        try
        {
            var st = await Core!.CallAsync("sync_status");
            var enabled = st.TryGetProperty("enabled", out var en) && en.GetBoolean();
            var lines = new List<string>();
            var attention = st.TryGetProperty("conflicts", out var c) && c.ValueKind == JsonValueKind.Number && c.GetInt64() > 0;
            var busy = false;
            var allPaused = true;
            if (enabled && st.TryGetProperty("syncs", out var syncs) && syncs.ValueKind == JsonValueKind.Array)
            {
                foreach (var s in syncs.EnumerateArray())
                {
                    var state = s.TryGetProperty("state", out var sv) ? sv.GetString() ?? "" : "";
                    var name = s.TryGetProperty("name", out var nv) ? nv.GetString() ?? "" : "";
                    lines.Add($"{name}: {SyncPage.StateText(s)}");
                    busy |= state == "syncing";
                    attention |= state is "conflict" or "held" or "offline" or "denied" or "disk_full" or "error";
                    allPaused &= s.TryGetProperty("paused", out var pv) && pv.ValueKind == JsonValueKind.True;
                }
            }
            var tip = attention ? "MediaLake: needs attention" : busy ? "MediaLake: syncing" : "MediaLake";
            _tray?.SetSync(lines.ToArray(), lines.Count > 0 && allPaused, tip);
        }
        catch { /* the next event tries again */ }
    }

    /// <summary>
    /// The uninstaller's step: no window, no single-instance check, and no core either. Starting it would
    /// start the model server and the syncs of an app that is being removed.
    /// </summary>
    private static void UnregisterSyncRootsAndExit()
    {
        try
        {
            var r = CoreClient.UnregisterSyncRoots(Environment.GetEnvironmentVariable("MEDIALAKE_HOME"));
            LogLine("unregister sync roots: " + r);
        }
        catch (Exception ex)
        {
            LogLine("unregister sync roots failed: " + ex.Message);
        }
        Environment.Exit(0);
    }

    /// <summary>The welcome window (shortcut and model, then sync); <paramref name="page"/> is 0 or 1.</summary>
    public void ShowWelcome(int page = 0)
    {
        if (_welcome == null)
        {
            _welcome = new WelcomeWindow();
            _welcome.Closed += (_, _) =>
            {
                _welcome = null;
                SearchChanged();
            };
        }
        _main?.Hide();
        _welcome.Open(page);
    }

    /// <summary>Search was set up or switched off: the search window re-reads its state.</summary>
    public void SearchChanged(bool reloadSettings = true)
    {
        _ = _main?.RefreshSearchStateAsync();
        // an open Settings window holds an older copy of the shortcut, the model and the search switch
        if (reloadSettings && _settings != null) _ = _settings.ReloadAsync();
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
            // MEDIALAKE_TEST_SYNC_DEMO=<folder>: this app hosts "Docs" (config.toml) and joins it itself as a
            // Cloud Files folder, so that the Sync page has something real to show
            if (Environment.GetEnvironmentVariable("MEDIALAKE_TEST_SYNC_DEMO") is { } point)
            {
                try
                {
                    var inv = await Core.CallAsync("sync_invite", new { grants = new[] { new { sync = "Docs", access = "rw" } } });
                    var paired = await Core.CallAsync("sync_pair", new { code = inv.GetProperty("code").GetString() });
                    var joined = await Core.CallAsync("sync_join", new { host = paired.GetProperty("host_id").GetString(), sync_id = "Docs", path = point, frontend = "auto", conflict = "ask" });
                    log.Add("joined=" + joined);
                    await Task.Delay(3000);
                    log.Add("browse=" + await Core.CallAsync("sync_browse", new { sync_id = "Docs", path = "" }));
                    // reading through Windows is what fetches a file
                    log.Add("read hello.txt=" + await Task.Run(() => File.ReadAllText(Path.Combine(point, "hello.txt"))));
                    await File.WriteAllTextAsync(Path.Combine(point, "from-pc.txt"), "made on this PC");
                    await Task.Delay(4000);
                    log.Add("browse after=" + await Core.CallAsync("sync_browse", new { sync_id = "Docs", path = "" }));
                    log.Add("sync_status=" + await Core.CallAsync("sync_status"));
                }
                catch (Exception ex) { log.Add("sync demo failed: " + ex.Message); }
            }
            if (Environment.GetEnvironmentVariable("MEDIALAKE_TEST_SETTINGS") is { } pagePath)
            {
                var parts = pagePath.Split('|');
                ShowSettings();
                await Task.Delay(1500);
                _settings!.SelectPage(int.Parse(parts[0]));
                await Task.Delay(1500);
                await SaveScreenshotAsync(_settings.RootElement, parts[1]);
                // MEDIALAKE_TEST_SYNC_DIALOGS=<folder>: the Sync page's dialogs, for the first synced folder
                if (Environment.GetEnvironmentVariable("MEDIALAKE_TEST_SYNC_DIALOGS") is { } shots)
                {
                    var root = _settings.RootElement.XamlRoot;
                    var hwnd = WinRT.Interop.WindowNative.GetWindowHandle(_settings);
                    var st = await Core.CallAsync("sync_status");
                    if (st.TryGetProperty("syncs", out var syncs) && syncs.GetArrayLength() > 0)
                    {
                        var s = syncs[0];
                        var cloud = s.GetProperty("frontend").GetString() == "cloud";
                        _ = SyncDialogs.BrowseAsync(root, s.GetProperty("sync_id").GetString()!, s.GetProperty("name").GetString()!, cloud, false);
                        log.Add("browse dialog=" + await ShootDialogAsync(root, Path.Combine(shots, "sync-browse.png")));
                    }
                    _ = SyncDialogs.JoinAsync(root, hwnd);
                    log.Add("join dialog=" + await ShootDialogAsync(root, Path.Combine(shots, "sync-join.png")));
                }
            }
            // MEDIALAKE_TEST_WELCOME=<folder>: the two pages of the welcome window
            if (Environment.GetEnvironmentVariable("MEDIALAKE_TEST_WELCOME") is { } dir)
            {
                ShowWelcome(0);
                await Task.Delay(1500);
                await SaveScreenshotAsync(_welcome!.RootElement, Path.Combine(dir, "welcome-search.png"));
                _welcome.Open(1);
                await Task.Delay(500);
                await SaveScreenshotAsync(_welcome.RootElement, Path.Combine(dir, "welcome-sync.png"));
            }
        }
        catch (Exception ex) { log.Add("FAIL " + ex); }
        File.WriteAllLines(Path.Combine(AppContext.BaseDirectory, "selftest.log"), log);
        Quit();
    }

    /// <summary>Renders the dialog that is open on <paramref name="root"/> and closes it. Returns what it found.</summary>
    private static async Task<string> ShootDialogAsync(Microsoft.UI.Xaml.XamlRoot root, string path)
    {
        await Task.Delay(1500);
        var found = new List<string>();
        foreach (var p in Microsoft.UI.Xaml.Media.VisualTreeHelper.GetOpenPopupsForXamlRoot(root))
        {
            found.Add(p.Child?.GetType().Name ?? "null");
            if (p.Child is not Microsoft.UI.Xaml.Controls.ContentDialog dlg) continue;
            await SaveScreenshotAsync(dlg, path);
            dlg.Hide();
            await Task.Delay(500);
        }
        return found.Count == 0 ? "no dialog" : string.Join(",", found);
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
