using System.Text.Json;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using MediaLake.Native;
using Windows.Storage.Pickers;

namespace MediaLake;

/// <summary>
/// The dialogs of the Sync page that the welcome window shows too: adding a folder from another computer,
/// sharing one from this PC, and the file browser of a synced folder (keep on this PC / free up space /
/// version history). They mirror the macOS sheets.
/// </summary>
internal static class SyncDialogs
{
    private static Task<JsonElement> Call(string method, object? args = null) => App.Core!.CallAsync(method, args);

    private static string Str(JsonElement e, string name) =>
        e.ValueKind == JsonValueKind.Object && e.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() ?? "" : "";

    private static long Num(JsonElement e, string name) =>
        e.ValueKind == JsonValueKind.Object && e.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.Number ? v.GetInt64() : 0;

    private static bool Flag(JsonElement e, string name) =>
        e.ValueKind == JsonValueKind.Object && e.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.True;

    internal static string Bytes(long b) => b switch
    {
        < 1024 => $"{b} B",
        < 1 << 20 => $"{b / 1024.0:0} KB",
        < 1 << 30 => $"{b / 1048576.0:0.0} MB",
        _ => $"{b / 1073741824.0:0.0} GB",
    };

    private static TextBlock Text(string text, double opacity = 1, double size = 0, bool semibold = false, bool critical = false)
    {
        var t = new TextBlock { Text = text, TextWrapping = TextWrapping.Wrap, Opacity = opacity };
        if (size > 0) t.FontSize = size;
        if (semibold) t.FontWeight = Microsoft.UI.Text.FontWeights.SemiBold;
        if (critical && Application.Current.Resources.TryGetValue("SystemFillColorCriticalBrush", out var c)) t.Foreground = (Brush)c;
        return t;
    }

    private static async Task<string?> PickFolderAsync(nint hwnd)
    {
        var picker = new FolderPicker { SuggestedStartLocation = PickerLocationId.DocumentsLibrary };
        picker.FileTypeFilter.Add("*");
        WinRT.Interop.InitializeWithWindow.Initialize(picker, hwnd);
        return (await picker.PickSingleFolderAsync())?.Path;
    }

    /// <summary>Where a synced folder goes unless the user picks another place.</summary>
    internal static string DefaultLocation(string name)
    {
        var safe = string.Join("-", name.Split(Path.GetInvalidFileNameChars(), StringSplitOptions.RemoveEmptyEntries)).Trim();
        return Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.UserProfile), "MediaLake", safe.Length > 0 ? safe : "Folder");
    }

    // ---------- add a folder from another computer ----------

    /// <summary>
    /// "Add a synced folder": pick a computer this PC is already paired with or paste an invitation code,
    /// then the folder, where it goes and how it behaves. Returns true if a folder was added.
    /// </summary>
    public static async Task<bool> JoinAsync(XamlRoot root, nint hwnd)
    {
        // sync has to be on before anything can be paired or joined
        await Call("sync_enable", new { enabled = true });

        var hostId = "";
        var joined = false;
        var available = new List<JsonElement>();
        var panel = new StackPanel { Spacing = 12, MinWidth = 500 };

        var hostCombo = new ComboBox { Header = "Computer", MinWidth = 420, Visibility = Visibility.Collapsed };
        var orText = Text("or paste an invitation code", opacity: 0.6, size: 12);
        orText.Visibility = Visibility.Collapsed;
        var codeBox = new TextBox { PlaceholderText = "SN1-…  (from \"Invite a device\" on the other computer)", Width = 380 };
        var connect = new Button { Content = "Connect", VerticalAlignment = VerticalAlignment.Bottom };
        var codeRow = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        codeRow.Children.Add(codeBox);
        codeRow.Children.Add(connect);

        var folders = new RadioButtons { Header = "Folder", Visibility = Visibility.Collapsed };
        var options = new StackPanel { Spacing = 12, Visibility = Visibility.Collapsed };
        var location = new TextBox { Header = "Put it in", Width = 380 };
        var choose = new Button { Content = "Choose…", VerticalAlignment = VerticalAlignment.Bottom };
        var locationRow = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        locationRow.Children.Add(location);
        locationRow.Children.Add(choose);
        var conflict = new ComboBox { Header = "When both sides changed a file", MinWidth = 420 };
        conflict.Items.Add(new ComboBoxItem { Content = "The other computer wins, mine is kept as a copy", Tag = "server_wins" });
        conflict.Items.Add(new ComboBoxItem { Content = "Ask me", Tag = "ask" });
        conflict.SelectedIndex = 0;
        var fullCopy = new CheckBox { Content = "Keep a full copy in a normal folder" };
        var copyHint = Text("", opacity: 0.7, size: 12);
        void ShowCopyHint() => copyHint.Text = fullCopy.IsChecked == true
            ? "Every file is downloaded and stored in the folder. Use this if the folder must also work without MediaLake running."
            : "Files are listed at once and downloaded when you open them. In Explorer, \"Always keep on this device\" keeps what you need offline.";
        fullCopy.Checked += (_, _) => ShowCopyHint();
        fullCopy.Unchecked += (_, _) => ShowCopyHint();
        ShowCopyHint();
        options.Children.Add(locationRow);
        options.Children.Add(conflict);
        options.Children.Add(fullCopy);
        options.Children.Add(copyHint);
        var error = Text("", critical: true, size: 12);

        panel.Children.Add(hostCombo);
        panel.Children.Add(orText);
        panel.Children.Add(codeRow);
        panel.Children.Add(folders);
        panel.Children.Add(options);
        panel.Children.Add(error);

        var dlg = new ContentDialog
        {
            Title = "Add a synced folder",
            Content = new ScrollViewer { Content = panel, VerticalScrollBarVisibility = ScrollBarVisibility.Auto },
            PrimaryButtonText = "Add",
            CloseButtonText = "Cancel",
            DefaultButton = ContentDialogButton.Primary,
            IsPrimaryButtonEnabled = false,
            XamlRoot = root,
        };

        JsonElement? Selected() => folders.SelectedItem is RadioButton { Tag: string id } ? available.FirstOrDefault(a => Str(a, "id") == id) : null;
        void Update() => dlg.IsPrimaryButtonEnabled = Selected() is { ValueKind: JsonValueKind.Object } && location.Text.Trim().Length > 0;

        async Task LoadAsync(string host)
        {
            error.Text = "";
            hostId = host;
            available.Clear();
            folders.Items.Clear();
            try
            {
                var res = await Call("sync_available", new { host });
                foreach (var s in res.GetProperty("syncs").EnumerateArray())
                {
                    available.Add(s);
                    var isJoined = Flag(s, "joined");
                    var info = $"{Str(s, "name")}  ·  {Num(s, "files"):N0} files, {Bytes(Num(s, "bytes"))}"
                               + (Str(s, "access") == "ro" ? "  ·  read only" : "") + (isJoined ? "  ·  already added" : "");
                    folders.Items.Add(new RadioButton { Content = info, Tag = Str(s, "id"), IsEnabled = !isJoined });
                }
                folders.Visibility = available.Count > 0 ? Visibility.Visible : Visibility.Collapsed;
                var first = folders.Items.OfType<RadioButton>().FirstOrDefault(r => r.IsEnabled);
                if (first != null) folders.SelectedItem = first;
                else error.Text = available.Count == 0 ? "This computer shares nothing with this PC." : "Every folder of this computer is already added.";
            }
            catch (Exception ex) { error.Text = ex.Message; }
            options.Visibility = Selected() != null ? Visibility.Visible : Visibility.Collapsed;
            Update();
        }

        folders.SelectionChanged += (_, _) =>
        {
            if (Selected() is { } s) location.Text = DefaultLocation(Str(s, "name"));
            options.Visibility = Selected() != null ? Visibility.Visible : Visibility.Collapsed;
            Update();
        };
        location.TextChanged += (_, _) => Update();
        choose.Click += async (_, _) =>
        {
            if (await PickFolderAsync(hwnd) is { } picked && Selected() is { } s) location.Text = Path.Combine(picked, Path.GetFileName(DefaultLocation(Str(s, "name"))));
        };
        hostCombo.SelectionChanged += async (_, _) =>
        {
            if (hostCombo.SelectedItem is ComboBoxItem { Tag: string id } && id.Length > 0) await LoadAsync(id);
        };
        connect.Click += async (_, _) =>
        {
            var code = codeBox.Text.Trim();
            if (code.Length == 0) return;
            connect.IsEnabled = false;
            error.Text = "";
            try
            {
                var paired = await Call("sync_pair", new { code });
                codeBox.Text = "";
                await LoadAsync(Str(paired, "host_id"));
            }
            catch (Exception ex) { error.Text = ex.Message; }
            connect.IsEnabled = true;
        };
        dlg.PrimaryButtonClick += async (_, e) =>
        {
            if (Selected() is not { } s) { e.Cancel = true; return; }
            var deferral = e.GetDeferral();
            try
            {
                await Call("sync_join", new
                {
                    host = hostId,
                    sync_id = Str(s, "id"),
                    path = location.Text.Trim(),
                    frontend = fullCopy.IsChecked == true ? "mirror" : "auto",
                    conflict = (conflict.SelectedItem as ComboBoxItem)?.Tag as string ?? "server_wins",
                });
                joined = true;
            }
            catch (Exception ex)
            {
                error.Text = ex.Message;
                e.Cancel = true; // stays open: the message says what to change
            }
            deferral.Complete();
        };

        // computers this PC is paired with already: another of their folders needs no new code
        try
        {
            var hosts = await Call("sync_hosts");
            if (hosts.TryGetProperty("hosts", out var hs) && hs.GetArrayLength() > 0)
            {
                hostCombo.Items.Add(new ComboBoxItem { Content = "Choose…", Tag = "" });
                foreach (var h in hs.EnumerateArray())
                    hostCombo.Items.Add(new ComboBoxItem { Content = $"{Str(h, "name")}  ({Str(h, "address")})", Tag = Str(h, "id") });
                hostCombo.SelectedIndex = 0;
                hostCombo.Visibility = orText.Visibility = Visibility.Visible;
            }
        }
        catch { /* the code field still works */ }

        await dlg.ShowAsync();
        return joined;
    }

    // ---------- share a folder from this PC ----------

    public static async Task<bool> ShareAsync(XamlRoot root, nint hwnd)
    {
        await Call("sync_enable", new { enabled = true });
        if (await PickFolderAsync(hwnd) is not { } folder) return false;
        var name = new TextBox { Header = "Name", Text = Path.GetFileName(folder.TrimEnd('\\')) };
        var backup = new ComboBox { Header = "Backups", MinWidth = 320 };
        foreach (var (label, tag) in new[]
                 {
                     ("Keep the last 10 versions of each file", "file:10"),
                     ("Keep the last 50 versions of each file", "file:50"),
                     ("Daily snapshots, keep 14", "time:24:14"),
                     ("None", "none"),
                 })
            backup.Items.Add(new ComboBoxItem { Content = label, Tag = tag });
        backup.SelectedIndex = 0;
        var error = Text("", critical: true, size: 12);
        var panel = new StackPanel { Spacing = 12 };
        panel.Children.Add(Text(folder, opacity: 0.7, size: 12));
        panel.Children.Add(name);
        panel.Children.Add(backup);
        panel.Children.Add(Text("The folder stays an ordinary folder. Versions and deleted files are kept inside .medialake-sync next to it.", opacity: 0.7, size: 12));
        panel.Children.Add(error);
        var shared = false;
        var dlg = new ContentDialog { Title = "Share a folder", Content = panel, PrimaryButtonText = "Share", CloseButtonText = "Cancel", DefaultButton = ContentDialogButton.Primary, XamlRoot = root };
        dlg.PrimaryButtonClick += async (_, e) =>
        {
            var deferral = e.GetDeferral();
            try
            {
                var policy = (backup.SelectedItem as ComboBoxItem)?.Tag as string ?? "file:10";
                await Call("sync_host_add", new { name = name.Text.Trim(), root = folder, backup = policy });
                shared = true;
            }
            catch (Exception ex)
            {
                error.Text = ex.Message;
                e.Cancel = true;
            }
            deferral.Complete();
        };
        await dlg.ShowAsync();
        return shared;
    }

    // ---------- browse: keep on this PC / free up space / history ----------

    private static string StateText(string state) => state switch
    {
        "cloud" => "Online only",
        "cached" or "local" => "On this PC",
        "dirty" => "Changed, not uploaded yet",
        _ => "",
    };

    /// <summary>
    /// The files of a synced folder with where each one is, and the earlier versions the sharing computer
    /// keeps. <paramref name="onDemand"/>: the folder loads files when they are opened (Cloud Files), so
    /// "keep" and "free up" mean something.
    /// </summary>
    public static async Task BrowseAsync(XamlRoot root, string syncId, string name, bool onDemand, bool readOnly)
    {
        var path = "";
        var header = Text("/", opacity: 0.6);
        var actions = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        var message = Text("", size: 12);
        var list = new StackPanel { Spacing = 2 };
        var panel = new StackPanel { Spacing = 10, Width = 640 };
        panel.Children.Add(header);
        panel.Children.Add(actions);
        panel.Children.Add(message);
        panel.Children.Add(new ScrollViewer { Content = list, Height = 380, VerticalScrollBarVisibility = ScrollBarVisibility.Auto });

        void Say(string text, bool bad = false)
        {
            message.Text = text;
            message.Foreground = bad && Application.Current.Resources.TryGetValue("SystemFillColorCriticalBrush", out var c) ? (Brush)c : header.Foreground;
            message.Opacity = bad ? 1 : 0.7;
        }

        Button Small(string text, Func<Task> action)
        {
            var b = new Button { Content = text, FontSize = 12, Padding = new Thickness(8, 3, 8, 3) };
            b.Click += async (_, _) =>
            {
                b.IsEnabled = false;
                try { await action(); }
                catch (Exception ex) { Say(ex.Message, bad: true); }
                b.IsEnabled = true;
            };
            return b;
        }

        async Task Pin(string rel, bool keep)
        {
            await Call(keep ? "sync_pin" : "sync_unpin", keep ? new { sync_id = syncId, path = rel } : new { sync_id = syncId, path = rel, evict = true });
            Say(keep ? "It is being downloaded and stays on this PC." : "The space is freed; the files stay listed.");
            await Task.Delay(400); // the work runs in the background: give the list something to show
            await ShowFolderAsync();
        }

        async Task ShowFolderAsync()
        {
            header.Text = "/" + path;
            actions.Children.Clear();
            if (path.Length > 0)
                actions.Children.Add(Small("Up", () =>
                {
                    path = path.Contains('/') ? path[..path.LastIndexOf('/')] : "";
                    return ShowFolderAsync();
                }));
            if (onDemand)
            {
                actions.Children.Add(Small("Always keep this folder on this PC", () => Pin(path, true)));
                actions.Children.Add(Small("Free up space here", () => Pin(path, false)));
            }
            list.Children.Clear();
            JsonElement res;
            try { res = await Call("sync_browse", new { sync_id = syncId, path }); }
            catch (Exception ex) { Say(ex.Message, bad: true); return; }
            var items = res.GetProperty("items").EnumerateArray().ToList();
            if (items.Count == 0) list.Children.Add(Text("This folder is empty.", opacity: 0.6));
            foreach (var it in items)
            {
                var (rel, isDir, state, pinned) = (Str(it, "path"), Flag(it, "is_dir"), Str(it, "state"), Flag(it, "pinned"));
                var row = new Grid { ColumnSpacing = 8, Padding = new Thickness(0, 2, 0, 2) };
                row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
                row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                var icon = new FontIcon { Glyph = isDir ? "" : "", FontSize = 14, VerticalAlignment = VerticalAlignment.Center };
                row.Children.Add(icon);
                FrameworkElement title;
                if (isDir)
                {
                    var open = new HyperlinkButton { Content = Str(it, "name"), Padding = new Thickness(0) };
                    open.Click += async (_, _) =>
                    {
                        path = rel;
                        Say("");
                        await ShowFolderAsync();
                    };
                    title = open;
                }
                else
                {
                    title = new TextBlock { Text = Str(it, "name"), TextTrimming = TextTrimming.CharacterEllipsis, VerticalAlignment = VerticalAlignment.Center };
                }
                Grid.SetColumn(title, 1);
                row.Children.Add(title);
                var info = new TextBlock
                {
                    Text = isDir ? (pinned ? "Always kept on this PC" : "") : $"{Bytes(Num(it, "size"))}  ·  {(pinned ? "Always kept on this PC" : StateText(state))}",
                    FontSize = 12,
                    Opacity = 0.6,
                    VerticalAlignment = VerticalAlignment.Center,
                };
                Grid.SetColumn(info, 2);
                row.Children.Add(info);
                var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
                if (onDemand)
                {
                    if (pinned || state == "cached") buttons.Children.Add(Small("Free up space", () => Pin(rel, false)));
                    if (!pinned && (state == "cloud" || isDir)) buttons.Children.Add(Small("Keep on this PC", () => Pin(rel, true)));
                }
                if (!isDir) buttons.Children.Add(Small("History", () => ShowHistoryAsync(rel)));
                Grid.SetColumn(buttons, 3);
                row.Children.Add(buttons);
                list.Children.Add(row);
            }
        }

        async Task ShowHistoryAsync(string rel)
        {
            header.Text = "History of " + Path.GetFileName(rel);
            actions.Children.Clear();
            actions.Children.Add(Small("Back", ShowFolderAsync));
            list.Children.Clear();
            Say("");
            var res = await Call("sync_versions", new { sync_id = syncId, path = rel });
            var versions = res.GetProperty("versions").EnumerateArray().ToList();
            if (versions.Count == 0) list.Children.Add(Text("No earlier versions are kept for this file.", opacity: 0.6));
            foreach (var v in versions)
            {
                var rev = Num(v, "rev");
                var when = DateTimeOffset.FromUnixTimeSeconds(Num(v, "created")).LocalDateTime;
                var row = new Grid { ColumnSpacing = 8, Padding = new Thickness(0, 2, 0, 2) };
                row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
                row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                row.Children.Add(new TextBlock { Text = $"{when:g}   {Bytes(Num(v, "size"))}  ·  {Str(v, "author")}", VerticalAlignment = VerticalAlignment.Center });
                async Task Restore(bool asCopy)
                {
                    await Call("sync_restore", new { sync_id = syncId, what = new { file = new { path = rel, rev } }, as_copy = asCopy });
                    await ShowFolderAsync();
                    Say(asCopy ? "The version was restored as a copy next to the file." : "The version was restored. The file changes here in a moment.");
                }
                var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 6 };
                var copy = Small("Restore as a copy", () => Restore(true));
                var over = Small("Restore", () => Restore(false));
                // a restore is a change on the sharing computer: read-only members cannot make one
                copy.IsEnabled = over.IsEnabled = !readOnly;
                buttons.Children.Add(copy);
                buttons.Children.Add(over);
                Grid.SetColumn(buttons, 1);
                row.Children.Add(buttons);
                list.Children.Add(row);
            }
            if (readOnly && versions.Count > 0) Say("This folder is read only: versions can be restored on a computer that may change it.");
        }

        var dlg = new ContentDialog { Title = name, Content = panel, CloseButtonText = "Done", XamlRoot = root };
        // the default ContentDialog is too narrow for a file list
        dlg.Resources["ContentDialogMaxWidth"] = 760.0;
        await ShowFolderAsync();
        await dlg.ShowAsync();
    }
}
