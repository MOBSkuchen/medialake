using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.UI;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Imaging;
using MediaLake.Native;
using Windows.Graphics;
using Windows.Storage;
using Windows.Storage.FileProperties;
using Windows.System;
using Windows.UI;

namespace MediaLake;

/// <summary>One row of the result list.</summary>
public sealed class ResultItem : INotifyPropertyChanged
{
    public string Path { get; set; } = "";
    public string Name { get; set; } = "";
    public string Folder { get; set; } = "";
    public string Snippet { get; set; } = "";
    public string Modality { get; set; } = "";
    public string ScoreText { get; set; } = "";
    public string Glyph { get; set; } = "";
    public Brush BadgeBrush { get; set; } = new SolidColorBrush(Color.FromArgb(255, 107, 114, 128));
    public Visibility SnippetVisibility => Snippet.Length > 0 ? Visibility.Visible : Visibility.Collapsed;

    private ImageSource? _thumb;
    public ImageSource? Thumb
    {
        get => _thumb;
        set { _thumb = value; PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(nameof(Thumb))); }
    }

    public event PropertyChangedEventHandler? PropertyChanged;

    private static SolidColorBrush Rgb(byte r, byte g, byte b) => new(Color.FromArgb(255, r, g, b));

    public static Brush BadgeFor(string modality) => modality switch
    {
        "text" => Rgb(0x2B, 0x7C, 0xD3),     // blue
        "document" => Rgb(0xC7, 0x70, 0x1F), // amber
        "image" => Rgb(0x2E, 0x9E, 0x5B),    // green
        "audio" => Rgb(0x8E, 0x5B, 0xD0),    // purple
        "video" => Rgb(0xD1, 0x4B, 0x64),    // rose
        _ => Rgb(0x6B, 0x72, 0x80),          // grey
    };

    public static ResultItem From(JsonElement r)
    {
        var path = r.GetProperty("path").GetString() ?? "";
        var modality = r.GetProperty("modality").GetString() ?? "other";
        return new ResultItem
        {
            Path = path,
            Name = r.GetProperty("name").GetString() ?? "",
            Folder = ShortenPath(System.IO.Path.GetDirectoryName(path) ?? ""),
            Snippet = r.GetProperty("snippet").GetString() ?? "",
            Modality = modality,
            ScoreText = $"{r.GetProperty("score").GetDouble() * 100:0}%",
            BadgeBrush = BadgeFor(modality),
            Glyph = modality switch
            {
                "image" => "",
                "audio" => "",
                "video" => "",
                "text" => "",
                "document" => "",
                _ => "",
            },
        };
    }

    private static string ShortenPath(string p)
    {
        const int max = 64;
        if (p.Length <= max) return p;
        var parts = p.Split('\\');
        if (parts.Length <= 3) return p;
        var tail = string.Join('\\', parts[^2..]);
        return parts[0] + "\\…\\" + tail;
    }
}

public sealed partial class MainWindow : Window
{
    public ObservableCollection<ResultItem> Items { get; } = [];

    private const int CollapsedHeight = 188; // title + input + status
    private const int ExpandedHeight = 580;
    private const int WindowWidth = 760;

    private readonly nint _hwnd;
    private int _searchId;
    private CancellationTokenSource? _debounce;
    private string _progressText = "";
    private string _serverText = "";
    private bool _everShown;
    private double _curHeight = CollapsedHeight; // DIPs
    private double _targetHeight = CollapsedHeight;
    private Microsoft.UI.Dispatching.DispatcherQueueTimer? _anim;
    private string _currentQuality = "normal";
    private bool _setupVisible;
    private bool _downloading;

    public MainWindow()
    {
        InitializeComponent();
        SystemBackdrop = new MicaBackdrop();
        _hwnd = WinRT.Interop.WindowNative.GetWindowHandle(this);

        var p = (OverlappedPresenter)AppWindow.Presenter;
        p.SetBorderAndTitleBar(false, false);
        p.IsResizable = false;
        p.IsMaximizable = false;
        p.IsMinimizable = false;
        p.IsAlwaysOnTop = true;
        AppWindow.IsShownInSwitchers = false;
        var round = 2; // DWMWCP_ROUND
        Win32.DwmSetWindowAttribute(_hwnd, 33, ref round, sizeof(int));
        // no 1px light border around the borderless window (DWMWA_BORDER_COLOR = DWMWA_COLOR_NONE)
        var none = unchecked((int)0xFFFFFFFE);
        Win32.DwmSetWindowAttribute(_hwnd, 34, ref none, sizeof(int));

        Activated += (_, e) =>
        {
            if (e.WindowActivationState == WindowActivationState.Deactivated && _everShown && !KeepOpen && !_downloading) Hide();
        };
        Closed += (_, e) => { e.Handled = true; Hide(); };
        RefreshStatus();
    }

    /// <summary>Set by tests/screenshots so the window does not hide on focus loss.</summary>
    public bool KeepOpen { get; set; }

    public nint Hwnd => _hwnd;

    // ---------- show / hide / size ----------

    private bool WantsExpanded => _setupVisible || QueryBox.Text.Trim().Length > 0;

    public void ShowSearch()
    {
        Win32.GetCursorPos(out var cur);
        var area = DisplayArea.GetFromPoint(new PointInt32(cur.X, cur.Y), DisplayAreaFallback.Nearest);
        var wa = area.WorkArea;
        var scale = GetScale();
        _targetHeight = WantsExpanded ? ExpandedHeight : CollapsedHeight;
        _curHeight = _targetHeight;
        var pw = (int)(WindowWidth * scale);
        var ph = (int)(_curHeight * scale);
        AppWindow.MoveAndResize(new RectInt32(wa.X + (wa.Width - pw) / 2, wa.Y + (int)(wa.Height * 0.16), pw, ph));
        AppWindow.Show();
        Activate();
        Win32.SetForegroundWindow(_hwnd);
        _everShown = true;
        QueryBox.Focus(FocusState.Programmatic);
        QueryBox.SelectAll();
        _ = CheckSetupAsync();
    }

    public void Hide() => AppWindow.Hide();

    public bool IsShown => AppWindow.IsVisible;

    private double GetScale() => Content?.XamlRoot?.RasterizationScale ?? 1.0;

    /// <summary>Grows the window while typing / shrinks it back when the query is cleared (animated).</summary>
    private void UpdateExpansion()
    {
        var target = WantsExpanded ? ExpandedHeight : CollapsedHeight;
        if (target == _targetHeight) return;
        _targetHeight = target;
        if (_anim == null)
        {
            _anim = DispatcherQueue.CreateTimer();
            _anim.Interval = TimeSpan.FromMilliseconds(15);
            _anim.Tick += (_, _) =>
            {
                var d = _targetHeight - _curHeight;
                _curHeight = Math.Abs(d) < 3 ? _targetHeight : _curHeight + d * 0.3;
                var pos = AppWindow.Position;
                var scale = GetScale();
                AppWindow.Resize(new SizeInt32((int)(WindowWidth * scale), (int)(_curHeight * scale)));
                if (_curHeight == _targetHeight) _anim!.Stop();
            };
        }
        if (!_anim.IsRunning) _anim.Start();
    }

    // ---------- search ----------

    private void Query_TextChanged(object sender, TextChangedEventArgs e)
    {
        _debounce?.Cancel();
        var cts = _debounce = new CancellationTokenSource();
        var q = QueryBox.Text.Trim();
        var id = Interlocked.Increment(ref _searchId);
        UpdateExpansion();
        if (_setupVisible) return;
        if (q.Length == 0)
        {
            Items.Clear();
            ShowEmpty("");
            LoadingBar.Visibility = Visibility.Collapsed;
            return;
        }
        LoadingBar.Visibility = Visibility.Visible;
        _ = RunSearchAsync(q, id, cts.Token);
    }

    private async Task RunSearchAsync(string query, int id, CancellationToken ct)
    {
        try
        {
            await Task.Delay(150, ct);
            var core = App.Core;
            if (core == null) { ShowEmpty("Starting…"); return; }
            var res = await core.CallAsync("search", new { query });
            if (id != _searchId) return; // a newer search superseded this one
            var items = res.GetProperty("results").EnumerateArray().Select(ResultItem.From).ToList();
            Items.Clear();
            foreach (var it in items) { Items.Add(it); _ = LoadThumbAsync(it); }
            if (items.Count > 0)
            {
                ResultsList.SelectedIndex = 0;
                ShowEmpty(null);
            }
            else ShowEmpty("No matches. Try different words, or add folders in Settings.");
        }
        catch (TaskCanceledException) { }
        catch (Exception ex)
        {
            if (id == _searchId) { Items.Clear(); ShowEmpty(ex.Message); }
        }
        finally
        {
            if (id == _searchId) LoadingBar.Visibility = Visibility.Collapsed;
        }
    }

    private void ShowEmpty(string? text)
    {
        EmptyText.Text = text ?? "";
        ResultsList.Visibility = text == null && !_setupVisible ? Visibility.Visible : Visibility.Collapsed;
    }

    private static async Task LoadThumbAsync(ResultItem item)
    {
        try
        {
            var f = await StorageFile.GetFileFromPathAsync(item.Path);
            using var t = await f.GetThumbnailAsync(ThumbnailMode.SingleItem, 96, ThumbnailOptions.UseCurrentScale);
            if (t == null || t.Type == ThumbnailType.Icon && t.Size == 0) return;
            var bmp = new BitmapImage();
            await bmp.SetSourceAsync(t);
            item.Thumb = bmp;
        }
        catch { /* no thumbnail: the glyph stays */ }
    }

    // ---------- first run: model download ----------

    private static string FormatSize(long bytes) =>
        bytes >= 1L << 30 ? $"{bytes / 1073741824.0:0.0} GB" : $"{bytes / 1048576.0:0} MB";

    /// <summary>Shows the model chooser/downloader when the current quality is not on disk yet.</summary>
    public async Task CheckSetupAsync()
    {
        var core = App.Core;
        if (core == null || _downloading) return;
        try
        {
            var cfg = await core.CallAsync("get_settings");
            var models = await core.CallAsync("models");
            var mode = cfg.GetProperty("server_mode").GetString();
            _currentQuality = cfg.GetProperty("quality").GetString() ?? "normal";
            var list = models.GetProperty("qualities").EnumerateArray().ToList();

            var selected = SetupQuality.SelectedItem is ComboBoxItem { Tag: string t } ? t : _currentQuality;
            SetupQuality.Items.Clear();
            foreach (var q in list)
            {
                var name = q.GetProperty("quality").GetString()!;
                var size = q.GetProperty("size").GetInt64();
                var label = name switch
                {
                    "heavy" => "Heavy: most precise",
                    "light" => "Light: smallest and fastest",
                    _ => "Normal: recommended",
                };
                var item = new ComboBoxItem { Content = $"{label}  ·  {FormatSize(size)}", Tag = name };
                SetupQuality.Items.Add(item);
                if (name == selected) SetupQuality.SelectedItem = item;
            }
            var current = list.FirstOrDefault(q => q.GetProperty("quality").GetString() == _currentQuality);
            var missing = mode == "local" && current.ValueKind == JsonValueKind.Object && !current.GetProperty("downloaded").GetBoolean();
            SetSetupVisible(missing);
        }
        catch { /* the status line shows core errors */ }
    }

    private void SetSetupVisible(bool visible)
    {
        _setupVisible = visible;
        SetupPanel.Visibility = visible ? Visibility.Visible : Visibility.Collapsed;
        if (visible)
        {
            ResultsList.Visibility = Visibility.Collapsed;
            EmptyText.Text = "";
        }
        UpdateExpansion();
    }

    private async void SetupDownload_Click(object sender, RoutedEventArgs e)
    {
        var core = App.Core;
        if (core == null || SetupQuality.SelectedItem is not ComboBoxItem { Tag: string quality }) return;
        _downloading = true;
        SetupButton.IsEnabled = false;
        SetupQuality.IsEnabled = false;
        SetupBar.Value = 0;
        SetupBar.Visibility = Visibility.Visible;
        SetupInfo.Text = "Starting download…";
        try
        {
            if (quality != _currentQuality)
            {
                var cfg = JsonNode.Parse((await core.CallAsync("get_settings")).GetRawText())!.AsObject();
                cfg["quality"] = quality;
                await core.CallAsync("set_settings", JsonSerializer.Deserialize<JsonElement>(cfg.ToJsonString()));
                _currentQuality = quality;
            }
            await core.CallAsync("download_model", new { quality });
            SetupInfo.Text = "";
        }
        catch (Exception ex)
        {
            SetupInfo.Text = ex.Message;
        }
        finally
        {
            _downloading = false;
            SetupButton.IsEnabled = true;
            SetupQuality.IsEnabled = true;
            SetupBar.Visibility = Visibility.Collapsed;
        }
        await CheckSetupAsync();
        if (!_setupVisible)
        {
            _serverText = "Model ready. Add folders to index in Settings.";
            RefreshStatus();
            QueryBox.Focus(FocusState.Programmatic);
        }
    }

    // ---------- keyboard / mouse ----------

    private void Query_PreviewKeyDown(object sender, KeyRoutedEventArgs e)
    {
        var ctrl = Microsoft.UI.Input.InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Control)
            .HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);
        switch (e.Key)
        {
            case VirtualKey.Escape:
                Hide();
                e.Handled = true;
                break;
            case VirtualKey.Down:
                Move(+1);
                e.Handled = true;
                break;
            case VirtualKey.Up:
                Move(-1);
                e.Handled = true;
                break;
            case VirtualKey.Enter:
                if (ResultsList.SelectedItem is ResultItem it)
                {
                    if (ctrl) ShellActions.ShowInExplorer(it.Path); else ShellActions.OpenFile(it.Path);
                    Hide();
                }
                e.Handled = true;
                break;
        }
    }

    private void Move(int delta)
    {
        if (Items.Count == 0) return;
        var i = Math.Clamp(ResultsList.SelectedIndex + delta, 0, Items.Count - 1);
        ResultsList.SelectedIndex = i;
        ResultsList.ScrollIntoView(Items[i]);
    }

    private void Results_ItemClick(object sender, ItemClickEventArgs e)
    {
        if (e.ClickedItem is ResultItem it)
        {
            ShellActions.OpenFile(it.Path);
            Hide();
        }
    }

    private void OpenInExplorer_Click(object sender, RoutedEventArgs e)
    {
        if (sender is Button { Tag: string path })
        {
            ShellActions.ShowInExplorer(path);
            Hide();
        }
    }

    private void Settings_Click(object sender, RoutedEventArgs e) => ((App)Application.Current).ShowSettings();

    // ---------- status line / progress ----------

    private static string FormatEta(ulong secs)
    {
        if (secs < 5) return "a few seconds";
        if (secs < 90) return $"~{secs} s";
        var min = (secs + 30) / 60;
        if (min < 90) return $"~{min} min";
        return $"~{min / 60} h {min % 60} min";
    }

    public void OnCoreEvent(JsonElement ev)
    {
        switch (ev.GetProperty("type").GetString())
        {
            case "progress":
                var queued = ev.GetProperty("queued").GetUInt64();
                var total = ev.GetProperty("total").GetUInt64();
                var done = ev.GetProperty("done").GetUInt64();
                var active = total > 0 && (queued > 0 || done < total);
                if (active)
                {
                    var text = $"Indexing {done:N0} / {total:N0}";
                    if (ev.TryGetProperty("eta_secs", out var eta) && eta.ValueKind == JsonValueKind.Number)
                        text += $"  ·  {FormatEta(eta.GetUInt64())} left";
                    _progressText = text;
                    IndexBar.Value = total > 0 ? (double)done / total : 0;
                    IndexBar.Visibility = Visibility.Visible;
                }
                else
                {
                    _progressText = "";
                    IndexBar.Visibility = Visibility.Collapsed;
                }
                break;
            case "idle":
                _progressText = $"{ev.GetProperty("files").GetUInt64():N0} files indexed";
                IndexBar.Visibility = Visibility.Collapsed;
                break;
            case "server":
                var state = ev.GetProperty("state").GetString();
                var detail = ev.GetProperty("detail").GetString();
                _serverText = state switch
                {
                    "starting" => "Model starting…",
                    "failed" => _setupVisible ? "" : string.IsNullOrEmpty(detail) ? "Embedding server failed" : detail!,
                    _ => "",
                };
                break;
            case "download":
                var d = ev.GetProperty("done").GetDouble();
                var t = ev.GetProperty("total").GetDouble();
                SetupBar.Value = t > 0 ? d / t : 0;
                SetupInfo.Text = $"{FormatSize((long)d)} of {FormatSize((long)t)}  ·  {(t > 0 ? d / t * 100 : 0):0}%";
                break;
            case "error":
                if (!_setupVisible) _serverText = ev.GetProperty("message").GetString() ?? "";
                break;
        }
        RefreshStatus();
    }

    private void RefreshStatus()
    {
        var parts = new[] { _serverText, _progressText }.Where(s => !string.IsNullOrEmpty(s));
        StatusText.Text = string.Join("  ·  ", parts);
    }

    public void SetQuery(string q) => QueryBox.Text = q;
    public FrameworkElement RootElement => Root;
}
