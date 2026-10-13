using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.Win32;
using MediaLake.Native;
using Windows.Graphics;
using Windows.System;

namespace MediaLake;

/// <summary>
/// "Welcome to MediaLake": page 1 sets up search (shortcut and model, both optional), page 2 sync
/// (optional too). Nothing is applied before a button is pressed: "Skip for now" sets up no shortcut and
/// downloads no model. Closing the window counts as done; the search window's hint leads back here.
/// </summary>
public sealed partial class WelcomeWindow : Window
{
    private const string RunKey = @"Software\Microsoft\Windows\CurrentVersion\Run";
    private const string RunValue = "MediaLake";

    private readonly Dictionary<string, List<(string Quality, long Size, bool Downloaded)>> _models = new();
    private readonly Dictionary<string, string> _modelNames = new();
    private bool _loading = true;
    private bool _recording;
    private bool _searchReady;
    private bool _downloading;
    /// A folder is synced or shared already: page 2 says "Done" instead of "Skip for now".
    private bool _joined;
    private string _shortcut = "";
    private int _page;

    public WelcomeWindow()
    {
        InitializeComponent();
        Closed += (_, _) => MarkDone();
    }

    public FrameworkElement RootElement => Root;

    /// <summary>Shows the window on page 0 (search) or 1 (sync).</summary>
    public void Open(int page)
    {
        Activate();
        var scale = Content?.XamlRoot?.RasterizationScale ?? 1.0;
        AppWindow.Resize(new SizeInt32((int)(720 * scale), (int)(740 * scale)));
        ShowPage(page);
        _ = LoadAsync();
    }

    private void ShowPage(int page)
    {
        _page = page;
        PageSearch.Visibility = page == 0 ? Visibility.Visible : Visibility.Collapsed;
        PageSync.Visibility = page == 1 ? Visibility.Visible : Visibility.Collapsed;
        BackButton.Visibility = page == 1 ? Visibility.Visible : Visibility.Collapsed;
        SkipButton.Visibility = page == 0 && !_downloading ? Visibility.Visible : Visibility.Collapsed;
        PrimaryButton.Content = page == 1 ? (_joined ? "Done" : "Skip for now") : _downloading ? "Next" : SelectedDownloaded() ? "Continue" : "Download";
    }

    // ---------- load ----------

    private async Task LoadAsync()
    {
        var core = App.Core;
        if (core == null) return;
        try
        {
            var cfg = await core.CallAsync("get_settings");
            var models = await core.CallAsync("models");
            var status = await core.CallAsync("status");
            _loading = true;
            _models.Clear();
            _modelNames.Clear();
            ModelCombo.Items.Clear();
            foreach (var e in models.GetProperty("models").EnumerateArray())
            {
                var id = e.GetProperty("id").GetString()!;
                _modelNames[id] = e.GetProperty("name").GetString() ?? id;
                _models[id] = e.GetProperty("qualities").EnumerateArray()
                    .Select(q => (q.GetProperty("quality").GetString()!, q.GetProperty("size").GetInt64(), q.GetProperty("downloaded").GetBoolean()))
                    .ToList();
                ModelCombo.Items.Add(new ComboBoxItem { Content = _modelNames[id], Tag = id });
            }
            var model = cfg.GetProperty("model").GetString() ?? "";
            foreach (ComboBoxItem it in ModelCombo.Items) if ((string?)it.Tag == model) ModelCombo.SelectedItem = it;
            if (ModelCombo.SelectedItem == null && ModelCombo.Items.Count > 0) ModelCombo.SelectedIndex = 0;
            FillQualities(cfg.GetProperty("quality").GetString() ?? "normal");
            // a single model needs no picker
            ModelCombo.Visibility = ModelCombo.Items.Count > 1 ? Visibility.Visible : Visibility.Collapsed;

            _shortcut = cfg.TryGetProperty("hotkey", out var h) ? h.GetString() ?? "" : "";
            ShowShortcut();
            AutostartSwitch.IsOn = Registry.CurrentUser.OpenSubKey(RunKey)?.GetValue(RunValue) != null;
            _searchReady = status.TryGetProperty("search", out var s) && s.GetString() == "ready";
            if (status.TryGetProperty("downloading", out var d) && d.GetBoolean()) _downloading = true;
        }
        catch (Exception ex)
        {
            DownloadText.Text = ex.Message;
        }
        _loading = false;
        await RefreshSyncListAsync();
    }

    private static string QualityLabel(string name) => name switch
    {
        "heavy" => "Heavy: most precise",
        "light" => "Light: smallest and fastest",
        _ => "Normal: recommended",
    };

    private static string FormatSize(long bytes) =>
        bytes >= 1L << 30 ? $"{bytes / 1073741824.0:0.0} GB" : $"{bytes / 1048576.0:0} MB";

    private void FillQualities(string select)
    {
        QualityCombo.Items.Clear();
        if (ModelCombo.SelectedItem is not ComboBoxItem { Tag: string id } || !_models.TryGetValue(id, out var list)) return;
        foreach (var (q, size, downloaded) in list)
        {
            var item = new ComboBoxItem { Content = $"{QualityLabel(q)}  ·  {(downloaded ? "on this PC" : FormatSize(size))}", Tag = q };
            QualityCombo.Items.Add(item);
            if (q == select) QualityCombo.SelectedItem = item;
        }
        if (QualityCombo.SelectedItem == null && QualityCombo.Items.Count > 0) QualityCombo.SelectedIndex = 0;
    }

    private string SelectedModel() => ModelCombo.SelectedItem is ComboBoxItem { Tag: string m } ? m : "";
    private string SelectedQuality() => QualityCombo.SelectedItem is ComboBoxItem { Tag: string q } ? q : "normal";

    private bool SelectedDownloaded() =>
        _models.TryGetValue(SelectedModel(), out var l) && l.Any(x => x.Quality == SelectedQuality() && x.Downloaded);

    private void Model_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_loading) return;
        FillQualities(SelectedQuality());
        ShowPage(_page);
    }

    // ---------- the shortcut: staged, registered only by "Download" ----------

    private void ShowShortcut()
    {
        HotkeyText.Text = _shortcut.Length == 0 ? "Not set" : _shortcut;
        HotkeyText.Opacity = _shortcut.Length == 0 ? 0.6 : 1;
        HotkeyRemove.Visibility = _shortcut.Length == 0 ? Visibility.Collapsed : Visibility.Visible;
    }

    private void HotkeyBox_Tapped(object sender, TappedRoutedEventArgs e) => HotkeyBox.Focus(FocusState.Programmatic);

    private void HotkeyBox_GotFocus(object sender, RoutedEventArgs e)
    {
        _recording = true;
        HotkeyHint.Text = "Press the combination…";
        HotkeyError.Text = "";
    }

    private void HotkeyBox_LostFocus(object sender, RoutedEventArgs e)
    {
        _recording = false;
        HotkeyHint.Text = "Click, then press the combination";
    }

    private static bool IsDown(VirtualKey k) =>
        Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(k).HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);

    private void HotkeyBox_KeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (!_recording) return;
        e.Handled = true;
        var key = e.Key;
        if (key is VirtualKey.Control or VirtualKey.Shift or VirtualKey.Menu or VirtualKey.LeftWindows or VirtualKey.RightWindows
            or VirtualKey.LeftControl or VirtualKey.RightControl or VirtualKey.LeftShift or VirtualKey.RightShift
            or VirtualKey.LeftMenu or VirtualKey.RightMenu)
            return; // wait for the main key
        var parts = new List<string>();
        if (IsDown(VirtualKey.Control)) parts.Add("Ctrl");
        if (IsDown(VirtualKey.Menu)) parts.Add("Alt");
        if (IsDown(VirtualKey.Shift)) parts.Add("Shift");
        if (IsDown(VirtualKey.LeftWindows) || IsDown(VirtualKey.RightWindows)) parts.Add("Win");
        if (parts.Count == 0)
        {
            HotkeyError.Text = "Include at least one of Ctrl, Alt, Shift or Win.";
            return;
        }
        var name = key.ToString();
        if (name.StartsWith("Number") && name.Length == 7) name = name[6..];
        parts.Add(name);
        var combo = string.Join('+', parts);
        if (!Hotkey.TryParse(combo, out _, out _))
        {
            HotkeyError.Text = "Use at least one modifier and one key, e.g. Ctrl+Alt+Space.";
            return;
        }
        HotkeyError.Text = "";
        _shortcut = combo;
        ShowShortcut();
        // a Grid does not take focus: move it to a real control so that the box stops recording
        PrimaryButton.Focus(FocusState.Programmatic);
    }

    private void HotkeyRemove_Click(object sender, RoutedEventArgs e)
    {
        _shortcut = "";
        HotkeyError.Text = "";
        ShowShortcut();
    }

    private void Autostart_Toggled(object sender, RoutedEventArgs e)
    {
        if (_loading) return;
        using var key = Registry.CurrentUser.CreateSubKey(RunKey);
        if (AutostartSwitch.IsOn)
            key.SetValue(RunValue, $"\"{Environment.ProcessPath}\" --background");
        else
            key.DeleteValue(RunValue, false);
    }

    // ---------- buttons ----------

    private async void Primary_Click(object sender, RoutedEventArgs e)
    {
        if (_page == 1) { Close(); return; }
        if (_downloading) { ShowPage(1); return; }
        await DownloadAsync();
    }

    private async void Skip_Click(object sender, RoutedEventArgs e)
    {
        var core = App.Core;
        // no shortcut, no download: search stays off until it is set up (the search window says so)
        if (core != null && !_searchReady)
        {
            try
            {
                var cfg = JsonNode.Parse((await core.CallAsync("get_settings")).GetRawText())!.AsObject();
                cfg["search_enabled"] = false;
                await core.CallAsync("set_settings", JsonSerializer.Deserialize<JsonElement>(cfg.ToJsonString()));
            }
            catch (Exception ex) { DownloadText.Text = ex.Message; }
        }
        ShowPage(1);
        ((App)Application.Current).SearchChanged();
    }

    private void Back_Click(object sender, RoutedEventArgs e) => ShowPage(0);

    private async Task DownloadAsync()
    {
        var core = App.Core;
        if (core == null) return;
        HotkeyError.Text = "";
        DownloadText.Text = "";
        // the shortcut is registered now, so a clash is shown here and not later
        var err = Hotkey.Register(_shortcut);
        if (err != null)
        {
            HotkeyError.Text = err;
            return;
        }
        var (model, quality) = (SelectedModel(), SelectedQuality());
        try
        {
            var cfg = JsonNode.Parse((await core.CallAsync("get_settings")).GetRawText())!.AsObject();
            cfg["hotkey"] = _shortcut;
            if (model.Length > 0) cfg["model"] = model;
            cfg["quality"] = quality;
            await core.CallAsync("set_settings", JsonSerializer.Deserialize<JsonElement>(cfg.ToJsonString()));
        }
        catch (Exception ex)
        {
            DownloadText.Text = ex.Message;
            return;
        }
        var needed = !SelectedDownloaded();
        ShowPage(1);
        if (needed)
        {
            _downloading = true;
            DownloadBar.Value = 0;
            DownloadBar.Visibility = Visibility.Visible;
            DownloadText.Text = "Starting download…";
        }
        ((App)Application.Current).SearchChanged();
        try
        {
            await core.CallAsync("download_model", new { model, quality });
            // search is switched on only now: nothing starts a server for a model that is not there
            var cfg = JsonNode.Parse((await core.CallAsync("get_settings")).GetRawText())!.AsObject();
            cfg["search_enabled"] = true;
            await core.CallAsync("set_settings", JsonSerializer.Deserialize<JsonElement>(cfg.ToJsonString()));
            DownloadText.Text = "Search is ready. Add the folders to search in Settings.";
        }
        catch (Exception ex)
        {
            DownloadText.Text = ex.Message;
        }
        _downloading = false;
        DownloadBar.Visibility = Visibility.Collapsed;
        await LoadAsync();
        ((App)Application.Current).SearchChanged();
    }

    /// <summary>Core events (UI thread): the model download's progress.</summary>
    public void OnCoreEvent(JsonElement ev)
    {
        if (ev.GetProperty("type").GetString() != "download") return;
        var d = ev.GetProperty("done").GetDouble();
        var t = ev.GetProperty("total").GetDouble();
        _downloading = true;
        DownloadBar.Visibility = Visibility.Visible;
        DownloadBar.Value = t > 0 ? d / t : 0;
        DownloadText.Text = $"Downloading the search model… {FormatSize((long)d)} of {FormatSize((long)t)}. You can close this window: the download continues in the background.";
    }

    // ---------- page 2: sync (the dialogs are the Sync page's) ----------

    private async void Join_Click(object sender, RoutedEventArgs e) =>
        await SyncActionAsync(() => SyncDialogs.JoinAsync(Content.XamlRoot, WinRT.Interop.WindowNative.GetWindowHandle(this)));

    private async void Share_Click(object sender, RoutedEventArgs e) =>
        await SyncActionAsync(() => SyncDialogs.ShareAsync(Content.XamlRoot, WinRT.Interop.WindowNative.GetWindowHandle(this)));

    private async Task SyncActionAsync(Func<Task<bool>> dialog)
    {
        if (App.Core == null) return;
        SyncInfo.Text = "";
        try { await dialog(); }
        catch (Exception ex) { SyncInfo.Text = ex.Message; }
        await RefreshSyncListAsync();
    }

    /// <summary>What is synced and shared already; with anything there, the page's button says "Done".</summary>
    private async Task RefreshSyncListAsync()
    {
        var core = App.Core;
        if (core == null) return;
        var lines = new List<string>();
        try
        {
            var st = await core.CallAsync("sync_status");
            if (st.TryGetProperty("syncs", out var syncs) && syncs.ValueKind == JsonValueKind.Array)
                foreach (var s in syncs.EnumerateArray())
                    lines.Add($"{s.GetProperty("name").GetString()} is synced to {s.GetProperty("path").GetString()}");
            if (st.TryGetProperty("hosting", out var h) && h.TryGetProperty("syncs", out var hosted) && hosted.ValueKind == JsonValueKind.Array)
                foreach (var s in hosted.EnumerateArray())
                    lines.Add($"{s.GetProperty("name").GetString()} is shared from this PC");
        }
        catch { /* sync is off: nothing to list */ }
        SyncList.Children.Clear();
        foreach (var l in lines)
        {
            var t = new TextBlock { Text = "✓  " + l, FontSize = 12, TextWrapping = TextWrapping.Wrap };
            if (Application.Current.Resources.TryGetValue("SystemFillColorSuccessBrush", out var ok)) t.Foreground = (Microsoft.UI.Xaml.Media.Brush)ok;
            SyncList.Children.Add(t);
        }
        _joined = lines.Count > 0;
        ShowPage(_page);
    }

    // ---------- leaving ----------

    /// <summary>The welcome has been dealt with, however it ended: it is not shown again by itself.</summary>
    private static void MarkDone()
    {
        try
        {
            var core = App.Core;
            if (core == null) return;
            var cfg = JsonNode.Parse(core.Call("get_settings").GetRawText())!.AsObject();
            if ((bool?)cfg["welcome_done"] == true) return;
            cfg["welcome_done"] = true;
            core.Call("set_settings", JsonSerializer.Deserialize<JsonElement>(cfg.ToJsonString()));
        }
        catch { /* the next start asks again */ }
    }
}
