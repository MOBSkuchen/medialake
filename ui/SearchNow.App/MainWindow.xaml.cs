using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Text.Json;
using Microsoft.UI;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Imaging;
using SearchNow.Native;
using Windows.Graphics;
using Windows.Storage;
using Windows.Storage.FileProperties;
using Windows.System;

namespace SearchNow;

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
    public Visibility SnippetVisibility => Snippet.Length > 0 ? Visibility.Visible : Visibility.Collapsed;

    private ImageSource? _thumb;
    public ImageSource? Thumb
    {
        get => _thumb;
        set { _thumb = value; PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(nameof(Thumb))); }
    }

    public event PropertyChangedEventHandler? PropertyChanged;

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

    private readonly nint _hwnd;
    private int _searchId;
    private CancellationTokenSource? _debounce;
    private string _progressText = "";
    private string _serverText = "";
    private bool _everShown;

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

        Activated += (_, e) =>
        {
            if (e.WindowActivationState == WindowActivationState.Deactivated && _everShown && !KeepOpen) Hide();
        };
        Closed += (_, e) => { e.Handled = true; Hide(); };
        RefreshStatus();
    }

    /// <summary>Set by tests/screenshots so the window does not hide on focus loss.</summary>
    public bool KeepOpen { get; set; }

    public nint Hwnd => _hwnd;

    // ---------- show / hide ----------

    public void ShowSearch()
    {
        var w = 760;
        var h = 560;
        Win32.GetCursorPos(out var cur);
        var area = DisplayArea.GetFromPoint(new PointInt32(cur.X, cur.Y), DisplayAreaFallback.Nearest);
        var wa = area.WorkArea;
        var scale = GetScale();
        var pw = (int)(w * scale);
        var ph = (int)(h * scale);
        AppWindow.MoveAndResize(new RectInt32(wa.X + (wa.Width - pw) / 2, wa.Y + (int)(wa.Height * 0.16), pw, ph));
        AppWindow.Show();
        Activate();
        Win32.SetForegroundWindow(_hwnd);
        _everShown = true;
        QueryBox.Focus(FocusState.Programmatic);
        QueryBox.SelectAll();
    }

    public void Hide()
    {
        AppWindow.Hide();
    }

    public bool IsShown => AppWindow.IsVisible;

    private double GetScale() => Content?.XamlRoot?.RasterizationScale ?? 1.0;

    // ---------- search ----------

    private void Query_TextChanged(object sender, TextChangedEventArgs e)
    {
        _debounce?.Cancel();
        var cts = _debounce = new CancellationTokenSource();
        var q = QueryBox.Text.Trim();
        var id = Interlocked.Increment(ref _searchId);
        if (q.Length == 0)
        {
            Items.Clear();
            ShowEmpty("");
            return;
        }
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
    }

    private void ShowEmpty(string? text)
    {
        EmptyText.Text = text ?? "";
        var showList = text == null;
        ResultsList.Visibility = showList ? Visibility.Visible : Visibility.Collapsed;
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

    // ---------- status line ----------

    public void OnCoreEvent(JsonElement ev)
    {
        switch (ev.GetProperty("type").GetString())
        {
            case "progress":
                var queued = ev.GetProperty("queued").GetUInt64();
                var total = ev.GetProperty("total").GetUInt64();
                var done = ev.GetProperty("done").GetUInt64();
                _progressText = total > 0 && (queued > 0 || done < total) ? $"Indexing {done:N0} / {total:N0}…" : "";
                break;
            case "idle":
                _progressText = $"{ev.GetProperty("files").GetUInt64():N0} files indexed";
                break;
            case "server":
                var state = ev.GetProperty("state").GetString();
                var detail = ev.GetProperty("detail").GetString();
                _serverText = state switch
                {
                    "starting" => "Model starting…",
                    "failed" => string.IsNullOrEmpty(detail) ? "Embedding server failed" : detail!,
                    _ => "",
                };
                break;
            case "error":
                _serverText = ev.GetProperty("message").GetString() ?? "";
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
