using System.ComponentModel;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.Win32;
using MediaLake.Native;
using Windows.Storage.Pickers;
using Windows.System;

namespace MediaLake;

public sealed class DirItem : INotifyPropertyChanged
{
    public string Path { get; set; } = "";
    public bool Recursive { get; set; } = true;
    public event PropertyChangedEventHandler? PropertyChanged;
    public void Raise() => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(null));
}

public sealed partial class SettingsWindow : Window
{
    private JsonObject _cfg = new();
    private bool _loading = true;
    private CancellationTokenSource? _saveDelay;
    private string _model = "embeddinggemma-2";
    private string _quality = "normal";
    /// Download state of the quality tiers: of every model, and of the selected one.
    private readonly Dictionary<string, Dictionary<string, (bool downloaded, long size)>> _allModels = new();
    private Dictionary<string, (bool downloaded, long size)> _models = new();
    private readonly System.Collections.ObjectModel.ObservableCollection<DirItem> _dirs = [];
    private bool _downloading;

    private const string RunKey = @"Software\Microsoft\Windows\CurrentVersion\Run";
    private const string RunValue = "MediaLake";

    public SettingsWindow()
    {
        InitializeComponent();
        PageSync.OwnerHwnd = WinRT.Interop.WindowNative.GetWindowHandle(this);
        SystemBackdrop = new Microsoft.UI.Xaml.Media.MicaBackdrop();
        AppWindow.Resize(new Windows.Graphics.SizeInt32(960, 720));
        AppWindow.SetIcon(System.IO.Path.Combine(AppContext.BaseDirectory, "Assets", "MediaLake.ico"));
        var none = unchecked((int)0xFFFFFFFE);
        Win32.DwmSetWindowAttribute(WinRT.Interop.WindowNative.GetWindowHandle(this), 34, ref none, sizeof(int));
        DirList.ItemsSource = _dirs;
        Nav.SelectedItem = Nav.MenuItems[0];
        _ = LoadAsync();
    }

    public FrameworkElement RootElement => Nav;
    internal FrameworkElement SyncContent => PageSync.Everything;
    public void SelectPage(int i) => Nav.SelectedItem = Nav.MenuItems[i];

    /// <summary>Opens the Sync page (a notification was clicked).</summary>
    public void SelectSync() => Nav.SelectedItem = Nav.MenuItems.OfType<NavigationViewItem>().First(i => (i.Tag as string) == "sync");

    // ---------- load / save ----------

    /// <summary>Re-reads the settings (the welcome window changed the shortcut, the model or the search switch).</summary>
    public Task ReloadAsync() => LoadAsync();

    private async Task LoadAsync()
    {
        var core = App.Core;
        if (core == null) return;
        try
        {
            _cfg = JsonNode.Parse((await core.CallAsync("get_settings")).GetRawText())!.AsObject();
            var models = await core.CallAsync("models");
            _loading = true;
            _allModels.Clear();
            ModelCombo.Items.Clear();
            foreach (var e in models.GetProperty("models").EnumerateArray())
            {
                var id = e.GetProperty("id").GetString()!;
                _allModels[id] = e.GetProperty("qualities").EnumerateArray().ToDictionary(
                    q => q.GetProperty("quality").GetString()!,
                    q => (q.GetProperty("downloaded").GetBoolean(), q.GetProperty("size").GetInt64()));
                var media = e.GetProperty("supports_audio").GetBoolean() ? "text, images, audio, video" : "text, images, video";
                ModelCombo.Items.Add(new ComboBoxItem { Content = $"{e.GetProperty("name").GetString()} ({media})", Tag = id });
            }
            _model = (string?)_cfg["model"] ?? "embeddinggemma-2";
            _models = _allModels.TryGetValue(_model, out var cur) ? cur : new();
        }
        catch (Exception ex)
        {
            HotkeyError.Text = ex.Message;
            return;
        }
        RefreshQualityLabels();
        SelectModelItem();
        _loading = true;
        SearchSwitch.IsOn = (bool?)_cfg["search_enabled"] ?? true;
        ShowHotkey((string?)_cfg["hotkey"] ?? "");
        AutostartSwitch.IsOn = Registry.CurrentUser.OpenSubKey(RunKey)?.GetValue(RunValue) != null;

        var mode = (string?)_cfg["server_mode"] ?? "local";
        ModeRadios.SelectedIndex = mode == "remote" ? 1 : 0;
        _quality = (string?)_cfg["quality"] ?? "normal";
        QualityCombo.SelectedIndex = _quality switch { "heavy" => 0, "light" => 2, _ => 1 };
        ThresholdSlider.Value = (double?)_cfg["threshold"] ?? 0.4;
        VideoSwitch.IsOn = (bool?)_cfg["video_enabled"] ?? false;
        RefreshQualityInfo();
        ShowModePanels(mode);
        RemoteQuality.Text = $"Quality is chosen by the server (currently {_quality}).";
        RemoteStatus.Text = _cfg["remote"] is JsonObject r ? $"Paired with {(string?)r["address"]}" : "Not connected";

        DebounceSlider.Value = ((double?)_cfg["debounce_ms"] ?? 2000) / 1000.0;
        MinChangeSlider.Value = ToSlider((long)((double?)_cfg["min_change_bytes"] ?? 512), 64, 1 << 20, zeroIsMin: true);
        MaxSizeSlider.Value = ToSlider((long)((double?)_cfg["max_file_bytes"] ?? 256L << 20), 1 << 20, 4L << 30, zeroIsMin: false);
        UpdateLabels();
        RebuildDirs();
        _loading = false;
        await RefreshStatsAsync();
    }

    private void RebuildDirs()
    {
        _dirs.Clear();
        if (_cfg["dirs"] is JsonArray a)
            foreach (var d in a)
                _dirs.Add(new DirItem { Path = (string?)d?["path"] ?? "", Recursive = (bool?)d?["recursive"] ?? true });
    }

    private async Task SaveAsync()
    {
        var core = App.Core;
        if (core == null || _loading) return;
        try
        {
            // the Sync page and the welcome window save their own settings: do not write back an older copy
            // of them (sync would be switched off again, the welcome flag would come back as false)
            var fresh = await core.CallAsync("get_settings");
            if (fresh.TryGetProperty("sync", out var sync)) _cfg["sync"] = JsonNode.Parse(sync.GetRawText());
            if (fresh.TryGetProperty("welcome_done", out var wd)) _cfg["welcome_done"] = wd.GetBoolean();
            var res = await core.CallAsync("set_settings", JsonSerializer.Deserialize<JsonElement>(_cfg.ToJsonString()));
            if (res.TryGetProperty("reindex", out var r) && r.GetBoolean()) await RefreshStatsAsync();
        }
        catch (Exception ex)
        {
            HotkeyError.Text = ex.Message;
        }
    }

    private async void SaveSoon()
    {
        if (_loading) return;
        _saveDelay?.Cancel();
        var cts = _saveDelay = new CancellationTokenSource();
        try
        {
            await Task.Delay(500, cts.Token);
            await SaveAsync();
        }
        catch (TaskCanceledException) { }
    }

    private async Task RefreshStatsAsync()
    {
        if (App.Core == null) return;
        try
        {
            var st = await App.Core.CallAsync("status");
            StatsText.Text = $"{st.GetProperty("files").GetUInt64():N0} files · {st.GetProperty("vectors").GetInt32():N0} vectors"
                + (st.GetProperty("busy").GetBoolean() ? " · indexing…" : "");
        }
        catch { }
    }

    public void OnCoreEvent(JsonElement ev)
    {
        PageSync.OnCoreEvent(ev);
        switch (ev.GetProperty("type").GetString())
        {
            case "download":
                var done = ev.GetProperty("done").GetDouble();
                var total = ev.GetProperty("total").GetDouble();
                DownloadBar.Value = total > 0 ? done / total : 0;
                DownloadText.Text = $"{done / 1048576:N0} / {total / 1048576:N0} MB";
                break;
            case "idle":
            case "progress":
                _ = RefreshStatsAsync();
                break;
        }
    }

    // ---------- navigation ----------

    private void Nav_SelectionChanged(NavigationView sender, NavigationViewSelectionChangedEventArgs args)
    {
        var tag = (args.SelectedItem as NavigationViewItem)?.Tag as string;
        PageGeneral.Visibility = tag == "general" ? Visibility.Visible : Visibility.Collapsed;
        PageModel.Visibility = tag == "model" ? Visibility.Visible : Visibility.Collapsed;
        PageIndex.Visibility = tag == "index" ? Visibility.Visible : Visibility.Collapsed;
        PageSync.Visibility = tag == "sync" ? Visibility.Visible : Visibility.Collapsed;
        if (tag == "index") _ = RefreshStatsAsync();
        if (tag == "sync") _ = PageSync.RefreshAsync();
    }

    // ---------- General ----------

    /// <summary>Search on or off. Off: MediaLake only syncs, no model is loaded and nothing is indexed.</summary>
    private async void Search_Toggled(object sender, RoutedEventArgs e)
    {
        var core = App.Core;
        if (_loading || core == null) return;
        var on = SearchSwitch.IsOn;
        if (on)
        {
            var ready = false;
            try { ready = (await core.CallAsync("status")).TryGetProperty("model_ready", out var m) && m.GetBoolean(); } catch { }
            if (!ready)
            {
                // there is no model to search with: setting it up is the way to switch search on
                _loading = true;
                SearchSwitch.IsOn = false;
                _loading = false;
                ((App)Application.Current).ShowWelcome(0);
                return;
            }
        }
        _cfg["search_enabled"] = on;
        await SaveAsync();
        ((App)Application.Current).SearchChanged(reloadSettings: false);
    }

    private bool _recording;

    private void HotkeyBox_Tapped(object sender, TappedRoutedEventArgs e) => HotkeyBox.Focus(FocusState.Programmatic);

    private void HotkeyBox_GotFocus(object sender, RoutedEventArgs e)
    {
        _recording = true;
        HotkeyHint.Text = "Press the new combination…";
        HotkeyError.Text = "";
    }

    private void HotkeyBox_LostFocus(object sender, RoutedEventArgs e)
    {
        _recording = false;
        HotkeyHint.Text = "Click, then press the new combination";
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
        var app = (App)Application.Current;
        var previous = (string?)_cfg["hotkey"] ?? "";
        var err = Hotkey.Register(combo); // validates by actually registering it
        if (err != null)
        {
            HotkeyError.Text = err;
            Hotkey.Register(previous);
            return;
        }
        HotkeyError.Text = "";
        ShowHotkey(combo);
        _cfg["hotkey"] = combo;
        app.ApplyHotkey(combo);
        _ = SaveAsync();
        Nav.Focus(FocusState.Programmatic);
    }

    /// <summary>"Not set" and a Remove button when there is no shortcut: nobody has to have one.</summary>
    private void ShowHotkey(string combo)
    {
        HotkeyText.Text = combo.Length == 0 ? "Not set" : combo;
        HotkeyText.Opacity = combo.Length == 0 ? 0.6 : 1;
        HotkeyRemove.Visibility = combo.Length == 0 ? Visibility.Collapsed : Visibility.Visible;
    }

    private void HotkeyRemove_Click(object sender, RoutedEventArgs e)
    {
        HotkeyError.Text = "";
        ShowHotkey("");
        _cfg["hotkey"] = "";
        ((App)Application.Current).ApplyHotkey("");
        _ = SaveAsync();
    }

    private async void KillDaemon_Click(object sender, RoutedEventArgs e)
    {
        var dlg = new ContentDialog
        {
            Title = "Quit MediaLake?",
            Content = "Stops the background process and the embedding server. The shortcut stops working until you start MediaLake again.",
            PrimaryButtonText = "Quit",
            CloseButtonText = "Cancel",
            XamlRoot = Content.XamlRoot,
        };
        if (await dlg.ShowAsync() == ContentDialogResult.Primary) ((App)Application.Current).QuitApp();
    }

    private async void Uninstall_Click(object sender, RoutedEventArgs e)
    {
        var uninstaller = Directory.EnumerateFiles(AppContext.BaseDirectory, "unins*.exe").FirstOrDefault();
        if (uninstaller == null)
        {
            await new ContentDialog
            {
                Title = "Uninstall",
                Content = "This copy was not installed with the MediaLake installer. Remove it by deleting its folder, or use Windows Settings > Apps.",
                CloseButtonText = "OK",
                XamlRoot = Content.XamlRoot,
            }.ShowAsync();
            return;
        }
        var dlg = new ContentDialog
        {
            Title = "Uninstall MediaLake?",
            Content = "MediaLake closes and the uninstaller starts. It asks whether to keep your index, settings and models.",
            PrimaryButtonText = "Uninstall",
            CloseButtonText = "Cancel",
            XamlRoot = Content.XamlRoot,
        };
        if (await dlg.ShowAsync() != ContentDialogResult.Primary) return;
        // leave the synced folders first: hydrated files stay as ordinary files, placeholders go, the
        // sync roots are unregistered (the uninstaller runs the same step once more, for the case that it
        // is started from Windows Settings)
        try { await App.Core!.CallAsync("sync_unregister_roots"); } catch { }
        System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo(uninstaller) { UseShellExecute = true });
        ((App)Application.Current).QuitApp();
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

    // ---------- Model ----------

    private void ShowModePanels(string mode)
    {
        LocalPanel.Visibility = mode == "local" ? Visibility.Visible : Visibility.Collapsed;
        RemotePanel.Visibility = mode == "remote" ? Visibility.Visible : Visibility.Collapsed;
    }

    private void Mode_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        var mode = (ModeRadios.SelectedItem as RadioButton)?.Tag as string ?? "local";
        ShowModePanels(mode);
        if (_loading) return;
        if (mode == "local" && (string?)_cfg["server_mode"] != "local")
        {
            _ = SwitchToLocalAsync();
        }
        else if (mode == "remote" && _cfg["remote"] == null)
        {
            RemoteStatus.Text = "Paste a pairing code and press Connect.";
        }
    }

    private async Task SwitchToLocalAsync()
    {
        try
        {
            await App.Core!.CallAsync("use_local");
            _cfg["server_mode"] = "local";
        }
        catch (Exception ex) { RemoteStatus.Text = ex.Message; }
    }

    private async void Connect_Click(object sender, RoutedEventArgs e)
    {
        var code = PairCode.Text.Trim();
        if (code.Length == 0) { RemoteStatus.Text = "Paste the pairing code first."; return; }
        ConnectButton.IsEnabled = false;
        RemoteStatus.Text = "Connecting…";
        try
        {
            var res = await App.Core!.CallAsync("pair_remote", new { code });
            _cfg["server_mode"] = "remote";
            RemoteStatus.Text = $"Connected to {res.GetProperty("address").GetString()} ({res.GetProperty("quality").GetString()}, {res.GetProperty("backend").GetString()})";
            PairCode.Text = "";
            if (res.TryGetProperty("reindex", out var r) && r.GetBoolean())
                RemoteStatus.Text += ". The index is being rebuilt with the server's model.";
        }
        catch (Exception ex) { RemoteStatus.Text = ex.Message; }
        finally { ConnectButton.IsEnabled = true; }
    }

    /// Size hints in the quality list (they differ per model).
    private void RefreshQualityLabels()
    {
        foreach (var o in QualityCombo.Items)
        {
            if (o is ComboBoxItem { Tag: string tag } ci && _models.TryGetValue(tag, out var info))
            {
                var label = tag switch { "heavy" => "Heavy: most precise", "light" => "Light: smallest and fastest", _ => "Normal: recommended" };
                var size = info.size >= 1L << 30 ? $"{info.size / 1073741824.0:0.0} GB" : $"{info.size / 1048576.0:0} MB";
                ci.Content = $"{label}  ·  {size}" + (info.downloaded ? " (downloaded)" : "");
            }
        }
    }

    private void SelectModelItem()
    {
        foreach (var o in ModelCombo.Items)
        {
            if (o is ComboBoxItem { Tag: string id } ci && id == _model)
            {
                ModelCombo.SelectedItem = ci;
                break;
            }
        }
    }

    private void RefreshQualityInfo()
    {
        if (_models.TryGetValue(_quality, out var m))
        {
            QualityInfo.Text = (m.downloaded ? "Downloaded" : $"Not downloaded yet ({m.size / 1048576:N0} MB). MediaLake cannot index or search until the model is present")
                + ". Changing the model or the quality re-indexes everything.";
            DownloadNow.Visibility = m.downloaded || _downloading ? Visibility.Collapsed : Visibility.Visible;
        }
    }

    private async void DownloadNow_Click(object sender, RoutedEventArgs e)
    {
        var tag = _quality;
        _downloading = true;
        DownloadNow.Visibility = Visibility.Collapsed;
        DownloadBar.Value = 0;
        DownloadBar.Visibility = DownloadText.Visibility = CancelDownload.Visibility = Visibility.Visible;
        QualityCombo.IsEnabled = ModelCombo.IsEnabled = false;
        try
        {
            await App.Core!.CallAsync("download_model", new { model = _model, quality = tag });
            _models[tag] = (true, _models[tag].size);
        }
        catch (Exception ex) { QualityInfo.Text = ex.Message; }
        finally
        {
            _downloading = false;
            DownloadBar.Visibility = DownloadText.Visibility = CancelDownload.Visibility = Visibility.Collapsed;
            QualityCombo.IsEnabled = ModelCombo.IsEnabled = true;
            RefreshQualityLabels();
            RefreshQualityInfo();
        }
    }

    private async void Quality_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_loading || _downloading) return;
        var tag = (QualityCombo.SelectedItem as ComboBoxItem)?.Tag as string;
        if (tag == null || tag == _quality) return;
        await SwitchAsync(_model, tag);
    }

    private async void Model_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_loading || _downloading) return;
        var id = (ModelCombo.SelectedItem as ComboBoxItem)?.Tag as string;
        if (id == null || id == _model) return;
        await SwitchAsync(id, _quality);
    }

    /// Switches model and/or quality after a confirmation (and a download when the files are missing).
    private async Task SwitchAsync(string model, string tag)
    {
        var modelChanged = model != _model;
        var tiers = _allModels.TryGetValue(model, out var t) ? t : new();
        var needsDownload = tiers.TryGetValue(tag, out var m) && !m.downloaded;
        var name = (ModelCombo.SelectedItem as ComboBoxItem)?.Content as string ?? model;
        var msg = needsDownload
            ? $"This needs a download of {m.size / 1048576:N0} MB ({tag} quality). Afterwards the whole index is rebuilt with the new model."
            : $"The whole index is rebuilt with the new {(modelChanged ? "model" : "quality")}. Searching keeps working while this happens.";
        var dlg = new ContentDialog
        {
            Title = modelChanged ? $"Switch to {name}?" : $"Switch to {tag} quality?",
            Content = msg,
            PrimaryButtonText = needsDownload ? "Download and switch" : "Switch",
            CloseButtonText = "Cancel",
            DefaultButton = ContentDialogButton.Primary,
            XamlRoot = Content.XamlRoot,
        };
        if (await dlg.ShowAsync() != ContentDialogResult.Primary)
        {
            RevertQuality();
            return;
        }
        if (needsDownload)
        {
            _downloading = true;
            DownloadBar.Value = 0;
            DownloadBar.Visibility = DownloadText.Visibility = CancelDownload.Visibility = Visibility.Visible;
            QualityCombo.IsEnabled = ModelCombo.IsEnabled = false;
            try
            {
                await App.Core!.CallAsync("download_model", new { model, quality = tag });
                tiers[tag] = (true, m.size);
            }
            catch (Exception ex)
            {
                QualityInfo.Text = ex.Message;
                RevertQuality();
                return;
            }
            finally
            {
                _downloading = false;
                DownloadBar.Visibility = DownloadText.Visibility = CancelDownload.Visibility = Visibility.Collapsed;
                QualityCombo.IsEnabled = ModelCombo.IsEnabled = true;
            }
        }
        _model = model;
        _quality = tag;
        _models = tiers;
        _cfg["model"] = model;
        _cfg["quality"] = tag;
        RefreshQualityLabels();
        RefreshQualityInfo();
        await SaveAsync();
        if (modelChanged)
        {
            // the score mapping follows the model
            try { _cfg = JsonNode.Parse((await App.Core!.CallAsync("get_settings")).GetRawText())!.AsObject(); }
            catch (Exception ex) { QualityInfo.Text = ex.Message; }
        }
    }

    private void RevertQuality()
    {
        _loading = true;
        QualityCombo.SelectedIndex = _quality switch { "heavy" => 0, "light" => 2, _ => 1 };
        SelectModelItem();
        _loading = false;
        RefreshQualityInfo();
    }

    private async void CancelDownload_Click(object sender, RoutedEventArgs e)
    {
        try { await App.Core!.CallAsync("cancel_download"); } catch { }
    }

    private void Video_Toggled(object sender, RoutedEventArgs e)
    {
        if (_loading) return;
        _cfg["video_enabled"] = VideoSwitch.IsOn;
        _ = SaveAsync();
    }

    // ---------- Index ----------

    private static double ToSlider(long v, double min, double max, bool zeroIsMin) =>
        v <= 0 && zeroIsMin ? 0 : Math.Clamp(Math.Log(Math.Max(v, min) / min) / Math.Log(max / min), 0, 1);

    private static long FromSlider(double s, double min, double max, bool zeroIsMin) =>
        zeroIsMin && s <= 0.001 ? 0 : (long)Math.Round(min * Math.Pow(max / min, s));

    private static string Bytes(long b) => b switch
    {
        0 => "0 B (re-embed on every change)",
        < 1024 => $"{b} B",
        < 1 << 20 => $"{b / 1024.0:0.#} KB",
        < 1 << 30 => $"{b / 1048576.0:0.#} MB",
        _ => $"{b / 1073741824.0:0.#} GB",
    };

    private void UpdateLabels()
    {
        DebounceLabel.Text = $"{DebounceSlider.Value:0.#} seconds after the last change";
        MinChangeLabel.Text = $"{Bytes(FromSlider(MinChangeSlider.Value, 64, 1 << 20, true))}. Smaller edits are collected until they add up.";
        MaxSizeLabel.Text = $"{Bytes(FromSlider(MaxSizeSlider.Value, 1 << 20, 4L << 30, false))}. Larger files are searchable by name only.";
    }

    private void Slider_ValueChanged(object sender, Microsoft.UI.Xaml.Controls.Primitives.RangeBaseValueChangedEventArgs e)
    {
        if (_loading || !IsLoaded()) return;
        UpdateLabels();
        _cfg["threshold"] = Math.Round(ThresholdSlider.Value, 2);
        _cfg["debounce_ms"] = (long)(DebounceSlider.Value * 1000);
        _cfg["min_change_bytes"] = FromSlider(MinChangeSlider.Value, 64, 1 << 20, true);
        _cfg["max_file_bytes"] = FromSlider(MaxSizeSlider.Value, 1 << 20, 4L << 30, false);
        SaveSoon();
    }

    private bool IsLoaded() => DebounceSlider != null && MaxSizeSlider != null && MinChangeSlider != null && ThresholdSlider != null;

    private async void AddDir_Click(object sender, RoutedEventArgs e)
    {
        var picker = new FolderPicker { SuggestedStartLocation = PickerLocationId.DocumentsLibrary };
        picker.FileTypeFilter.Add("*");
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
        var folder = await picker.PickSingleFolderAsync();
        if (folder == null) return;
        try
        {
            await App.Core!.CallAsync("add_dir", new { path = folder.Path, recursive = true });
            _cfg["dirs"] = JsonNode.Parse((await App.Core.CallAsync("get_settings")).GetProperty("dirs").GetRawText());
            RebuildDirs();
            await RefreshStatsAsync();
        }
        catch (Exception ex) { StatsText.Text = ex.Message; }
    }

    private async void RemoveDir_Click(object sender, RoutedEventArgs e)
    {
        if (sender is not Button { Tag: string path }) return;
        try
        {
            await App.Core!.CallAsync("remove_dir", new { path });
            _cfg["dirs"] = JsonNode.Parse((await App.Core.CallAsync("get_settings")).GetProperty("dirs").GetRawText());
            RebuildDirs();
            await RefreshStatsAsync();
        }
        catch (Exception ex) { StatsText.Text = ex.Message; }
    }

    private async void Recursive_Click(object sender, RoutedEventArgs e)
    {
        if (sender is not CheckBox { Tag: string path } cb) return;
        try
        {
            await App.Core!.CallAsync("add_dir", new { path, recursive = cb.IsChecked == true });
            _cfg["dirs"] = JsonNode.Parse((await App.Core.CallAsync("get_settings")).GetProperty("dirs").GetRawText());
        }
        catch (Exception ex) { StatsText.Text = ex.Message; }
    }

    private async void Reindex_Click(object sender, RoutedEventArgs e)
    {
        var dlg = new ContentDialog
        {
            Title = "Reindex everything?",
            Content = "All entries are rebuilt from your files. Searching keeps working while this happens.",
            PrimaryButtonText = "Reindex",
            CloseButtonText = "Cancel",
            XamlRoot = Content.XamlRoot,
        };
        if (await dlg.ShowAsync() != ContentDialogResult.Primary) return;
        try { await App.Core!.CallAsync("reindex_all"); await RefreshStatsAsync(); }
        catch (Exception ex) { StatsText.Text = ex.Message; }
    }
}
