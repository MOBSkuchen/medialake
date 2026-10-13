using System.Text.Json;
using System.Text.Json.Nodes;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using MediaLake.Native;
using Windows.ApplicationModel.DataTransfer;
using Windows.Storage.Pickers;

namespace MediaLake;

/// <summary>
/// Settings > Sync: joined folders, conflicts, folders shared from this PC, devices, speed limits.
/// Mirrors the macOS page; everything goes through the core's <c>sync_*</c> calls. Pinning files and
/// freeing space are done in Explorer (the folder buttons here are shortcuts for a whole sync).
/// </summary>
public sealed partial class SyncPage : UserControl
{
    private bool _building;
    private string _message = "";
    private DateTime _lastEvent;
    private bool _refreshQueued;

    // what the limit controls show (0 = unlimited); remembered so that the switch brings the number back
    private long _up, _down;
    private long _rememberedUp = 8000, _rememberedDown = 8000;
    private string _deviceName = "";

    /// The window this page sits in (the folder picker needs it).
    public nint OwnerHwnd { get; set; }

    public SyncPage()
    {
        InitializeComponent();
    }

    /// <summary>Core events (UI thread): refresh soon, but not once per file.</summary>
    public void OnCoreEvent(JsonElement ev)
    {
        var t = ev.GetProperty("type").GetString() ?? "";
        if (!t.StartsWith("sync_") || _refreshQueued) return;
        _refreshQueued = true;
        _ = Task.Run(async () =>
        {
            await Task.Delay(400);
            DispatcherQueue.TryEnqueue(async () =>
            {
                _refreshQueued = false;
                await RefreshAsync();
            });
        });
    }

    // ---------- loading and drawing ----------

    public async Task RefreshAsync()
    {
        var core = App.Core;
        if (core == null || _building) return;
        _building = true;
        try
        {
            var st = await core.CallAsync("sync_status");
            var enabled = st.TryGetProperty("enabled", out var en) && en.GetBoolean();
            JsonElement conflicts = default, clients = default;
            if (enabled)
            {
                try { conflicts = await core.CallAsync("sync_conflicts"); } catch { }
                try { clients = await core.CallAsync("sync_clients"); } catch { }
            }
            var settings = await core.CallAsync("get_settings");
            if (settings.TryGetProperty("sync", out var sync))
            {
                _deviceName = Str(sync, "device_name");
                _up = Num(sync, "upload_limit_kbps");
                _down = Num(sync, "download_limit_kbps");
                if (_up > 0) _rememberedUp = _up;
                if (_down > 0) _rememberedDown = _down;
            }
            Draw(st, enabled, conflicts, clients);
        }
        catch (Exception ex)
        {
            Host.Children.Clear();
            Host.Children.Add(Text("Sync", style: "TitleTextBlockStyle"));
            Host.Children.Add(Text(ex.Message, critical: true));
        }
        finally
        {
            _building = false;
        }
    }

    private static string Str(JsonElement e, string name) =>
        e.ValueKind == JsonValueKind.Object && e.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() ?? "" : "";

    private static long Num(JsonElement e, string name) =>
        e.ValueKind == JsonValueKind.Object && e.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.Number ? v.GetInt64() : 0;

    private static bool Flag(JsonElement e, string name) =>
        e.ValueKind == JsonValueKind.Object && e.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.True;

    private static string Bytes(long b) => b switch
    {
        < 1024 => $"{b} B",
        < 1 << 20 => $"{b / 1024.0:0} KB",
        < 1 << 30 => $"{b / 1048576.0:0.0} MB",
        _ => $"{b / 1073741824.0:0.0} GB",
    };

    /// <summary>One line for a sync's state (the tray menu uses it too).</summary>
    internal static string StateText(JsonElement s)
    {
        var state = Str(s, "state");
        var conflicts = Num(s, "pending_conflicts");
        return state switch
        {
            "idle" => Num(s, "unsynced_files") > 0 ? $"{Num(s, "unsynced_files")} change(s) not uploaded yet" : "Up to date",
            "syncing" => "Syncing…",
            "starting" => "Starting…",
            "paused" => "Paused",
            "conflict" => $"{Math.Max(conflicts, 1)} conflict(s) need a decision",
            "held" => "Deletions are waiting for your confirmation",
            "offline" => "Host not reachable",
            "denied" => "Access to this folder was withdrawn",
            "disk_full" => "This PC's disk is full",
            _ => Str(s, "detail") is { Length: > 0 } d ? d : "Something went wrong",
        };
    }

    // small builders, so that the page reads as a list of what is on it
    private static TextBlock Text(string text, bool semibold = false, double opacity = 1, string? style = null, bool critical = false, bool caution = false, double size = 0)
    {
        var t = new TextBlock { Text = text, TextWrapping = TextWrapping.Wrap, Opacity = opacity };
        if (semibold) t.FontWeight = Microsoft.UI.Text.FontWeights.SemiBold;
        if (size > 0) t.FontSize = size;
        if (style != null && Application.Current.Resources.TryGetValue(style, out var s) && s is Style st) t.Style = st;
        if (critical && Application.Current.Resources.TryGetValue("SystemFillColorCriticalBrush", out var c)) t.Foreground = (Brush)c;
        if (caution && Application.Current.Resources.TryGetValue("SystemFillColorCautionBrush", out var w)) t.Foreground = (Brush)w;
        return t;
    }

    private Button Btn(string text, Func<Task> action, bool accent = false)
    {
        var b = new Button { Content = text };
        if (accent && Application.Current.Resources.TryGetValue("AccentButtonStyle", out var s) && s is Style st) b.Style = st;
        b.Click += async (_, _) =>
        {
            try
            {
                await action();
                _message = "";
            }
            catch (Exception ex)
            {
                _message = ex.Message;
            }
            await RefreshAsync();
        };
        return b;
    }

    private static Border Card(UIElement inner)
    {
        var b = new Border { CornerRadius = new CornerRadius(8), Padding = new Thickness(14), Child = inner };
        if (Application.Current.Resources.TryGetValue("CardBackgroundFillColorDefaultBrush", out var bg)) b.Background = (Brush)bg;
        return b;
    }

    private static StackPanel Row(params UIElement[] items)
    {
        var p = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        foreach (var i in items) p.Children.Add(i);
        return p;
    }

    private static Task Call(string method, object? args = null) => App.Core!.CallAsync(method, args);

    private void Draw(JsonElement st, bool enabled, JsonElement conflicts, JsonElement clients)
    {
        Host.Children.Clear();
        Host.Children.Add(Text("Sync", style: "TitleTextBlockStyle"));

        var toggle = new ToggleSwitch { Header = "Keep folders in sync with other computers", IsOn = enabled };
        toggle.Toggled += async (_, _) =>
        {
            if (_building) return;
            try { await Call("sync_enable", new { enabled = toggle.IsOn }); _message = ""; }
            catch (Exception ex) { _message = ex.Message; }
            await RefreshAsync();
        };
        Host.Children.Add(toggle);
        Host.Children.Add(Text("A synced folder appears under your user folder > MediaLake, lists everything at once and loads a file when you open it. Changes you make are sent to the computer that shares the folder, and from there to everyone else.", opacity: 0.7));
        if (_message.Length > 0) Host.Children.Add(Text(_message, critical: true));
        if (!enabled) return;

        // ---- my folders ----
        Host.Children.Add(Text("My folders", semibold: true));
        var syncs = st.TryGetProperty("syncs", out var sy) && sy.ValueKind == JsonValueKind.Array ? sy.EnumerateArray().ToList() : new List<JsonElement>();
        if (syncs.Count == 0) Host.Children.Add(Text("No synced folders yet", opacity: 0.6));
        foreach (var s in syncs) Host.Children.Add(FolderCard(s));
        var addRow = Row(Btn("Add a folder…", () =>
        {
            ((App)Application.Current).ShowWelcome(1);
            return Task.CompletedTask;
        }));
        if (syncs.Count > 1)
        {
            var allPaused = syncs.All(s => Flag(s, "paused"));
            addRow.Children.Add(Btn(allPaused ? "Resume all" : "Pause all", () => Call(allPaused ? "sync_resume" : "sync_pause")));
        }
        Host.Children.Add(addRow);

        // ---- conflicts ----
        if (conflicts.ValueKind == JsonValueKind.Object && conflicts.TryGetProperty("conflicts", out var cs) && cs.GetArrayLength() > 0)
        {
            Host.Children.Add(Text("Conflicts", semibold: true));
            Host.Children.Add(Text("These files were changed here and on the other computer. Pick what to keep.", opacity: 0.7));
            foreach (var c in cs.EnumerateArray())
            {
                var (sid, path) = (Str(c, "sync_id"), Str(c, "path"));
                var inner = new StackPanel { Spacing = 6 };
                inner.Children.Add(Text(path, semibold: true));
                inner.Children.Add(Text(Flag(c, "host_deleted") ? $"{Str(c, "name")}: deleted on the other computer, edited here" : $"{Str(c, "name")}: edited in both places", opacity: 0.7, size: 12));
                inner.Children.Add(Row(
                    Btn("Keep mine", () => Call("sync_resolve", new { sync_id = sid, path, choice = "mine" })),
                    Btn("Use theirs", () => Call("sync_resolve", new { sync_id = sid, path, choice = "server" })),
                    Btn("Keep both", () => Call("sync_resolve", new { sync_id = sid, path, choice = "both" }))));
                Host.Children.Add(Card(inner));
            }
        }

        // ---- sharing ----
        Host.Children.Add(Text("Sharing from this PC", semibold: true));
        var hosted = new List<JsonElement>();
        if (st.TryGetProperty("hosting", out var h) && h.ValueKind == JsonValueKind.Object)
        {
            if (Str(h, "error") is { Length: > 0 } herr) Host.Children.Add(Text(herr, critical: true));
            else if (Flag(h, "running")) Host.Children.Add(Text($"Other computers reach this PC at {Str(h, "public_addr")}.", opacity: 0.7));
            if (h.TryGetProperty("syncs", out var hs) && hs.ValueKind == JsonValueKind.Array) hosted = hs.EnumerateArray().ToList();
        }
        if (hosted.Count == 0) Host.Children.Add(Text("Nothing is shared yet", opacity: 0.6));
        foreach (var hsync in hosted)
        {
            var id = Str(hsync, "id");
            var inner = new StackPanel { Spacing = 4 };
            inner.Children.Add(Text(Str(hsync, "name"), semibold: true));
            inner.Children.Add(Text(Str(hsync, "root"), opacity: 0.7, size: 12));
            inner.Children.Add(Text($"{Num(hsync, "files"):N0} files · {Bytes(Num(hsync, "bytes"))} · backups: {Str(hsync, "backup")}", opacity: 0.7, size: 12));
            if (hsync.TryGetProperty("issues", out var iss) && iss.ValueKind == JsonValueKind.Array)
                foreach (var i in iss.EnumerateArray()) inner.Children.Add(Text(i.GetString() ?? "", caution: true, size: 12));
            inner.Children.Add(Row(
                Btn("Snapshot now", () => Call("sync_host_snapshot", new { sync = id })),
                Btn("Stop sharing", () => Call("sync_host_remove", new { sync = id }))));
            Host.Children.Add(Card(inner));
        }
        var shareRow = Row(Btn("Share a folder…", ShareFolderAsync));
        var inviteBtn = Btn("Invite a device…", () => InviteAsync(hosted));
        inviteBtn.IsEnabled = hosted.Count > 0;
        shareRow.Children.Add(inviteBtn);
        Host.Children.Add(shareRow);
        Host.Children.Add(Text("A laptop is a poor host: other computers cannot exchange changes while it sleeps. For always-on sharing, run medialake-server on a computer that stays on.", opacity: 0.7, size: 12));

        // ---- devices ----
        if (clients.ValueKind == JsonValueKind.Object && clients.TryGetProperty("clients", out var cl) && cl.GetArrayLength() > 0)
        {
            Host.Children.Add(Text("Devices", semibold: true));
            foreach (var c in cl.EnumerateArray())
            {
                var cid = Str(c, "id");
                var inner = new StackPanel { Spacing = 6 };
                inner.Children.Add(Row(Text(Str(c, "name"), semibold: true), Btn("Remove device", () => Call("sync_revoke", new { client = cid }))));
                var access = new Dictionary<string, string>();
                if (c.TryGetProperty("syncs", out var cs2) && cs2.ValueKind == JsonValueKind.Object)
                    foreach (var p in cs2.EnumerateObject()) access[p.Name] = p.Value.GetString() ?? "";
                foreach (var hsync in hosted)
                {
                    var name = Str(hsync, "name");
                    var combo = new ComboBox { MinWidth = 160 };
                    foreach (var (label, tag) in new[] { ("No access", "none"), ("Read only", "ro"), ("Read and write", "rw") })
                        combo.Items.Add(new ComboBoxItem { Content = label, Tag = tag });
                    var current = access.TryGetValue(name, out var a) ? a : "none";
                    combo.SelectedIndex = current == "rw" ? 2 : current == "ro" ? 1 : 0;
                    combo.SelectionChanged += async (_, _) =>
                    {
                        if (_building || combo.SelectedItem is not ComboBoxItem { Tag: string tag }) return;
                        try { await Call("sync_grant", new { client = cid, sync = name, access = tag }); }
                        catch (Exception ex) { _message = ex.Message; }
                        await RefreshAsync();
                    };
                    inner.Children.Add(Row(new TextBlock { Text = name, MinWidth = 140, VerticalAlignment = VerticalAlignment.Center, Opacity = 0.8 }, combo));
                }
                Host.Children.Add(Card(inner));
            }
        }

        // ---- this device ----
        Host.Children.Add(Text("This device", semibold: true));
        var nameBox = new TextBox { Header = "Name shown to others", Text = _deviceName, Width = 320 };
        Host.Children.Add(nameBox);
        var (upSwitch, upBox) = LimitControls("Limit upload speed", _up, _rememberedUp);
        var (downSwitch, downBox) = LimitControls("Limit download speed", _down, _rememberedDown);
        Host.Children.Add(Row(upSwitch, upBox));
        Host.Children.Add(Row(downSwitch, downBox));
        Host.Children.Add(Btn("Save", async () =>
        {
            var s = JsonNode.Parse((await Call("get_settings")).GetRawText())!.AsObject();
            var sync = s["sync"]!.AsObject();
            sync["device_name"] = nameBox.Text;
            sync["upload_limit_kbps"] = upSwitch.IsOn ? (long)Math.Max(upBox.Value, 1) : 0;
            sync["download_limit_kbps"] = downSwitch.IsOn ? (long)Math.Max(downBox.Value, 1) : 0;
            await Call("set_settings", JsonSerializer.Deserialize<JsonElement>(s.ToJsonString()));
        }));
    }

    /// <summary>A speed limit with an on/off switch: off = unlimited (stored as 0), on shows the number.</summary>
    private (ToggleSwitch, NumberBox) LimitControls(string title, long current, long remembered)
    {
        var sw = new ToggleSwitch { Header = title, IsOn = current > 0, MinWidth = 200 };
        var box = new NumberBox
        {
            Value = current > 0 ? current : remembered,
            Minimum = 1,
            Maximum = 10_000_000,
            Header = "kbit/s",
            Width = 140,
            Visibility = current > 0 ? Visibility.Visible : Visibility.Collapsed,
            SpinButtonPlacementMode = NumberBoxSpinButtonPlacementMode.Hidden,
        };
        sw.Toggled += (_, _) => box.Visibility = sw.IsOn ? Visibility.Visible : Visibility.Collapsed;
        return (sw, box);
    }

    private UIElement FolderCard(JsonElement s)
    {
        var id = Str(s, "sync_id");
        var path = Str(s, "path");
        var paused = Flag(s, "paused");
        var needsAttention = Str(s, "state") is "conflict" or "held" or "offline" or "denied" or "disk_full" or "error";
        var inner = new StackPanel { Spacing = 6 };
        var head = Row(Text(Str(s, "name"), semibold: true), Text(StateText(s), opacity: needsAttention ? 1 : 0.75, caution: needsAttention));
        if (Str(s, "access") != "rw") head.Children.Add(Text("Read only", opacity: 0.6, size: 12));
        inner.Children.Add(head);
        inner.Children.Add(Text(path, opacity: 0.7, size: 12));
        var host = Str(s, "host_name");
        if (host.Length > 0) inner.Children.Add(Text($"From {host}", opacity: 0.6, size: 12));
        if (Str(s, "notice") is { Length: > 0 } notice) inner.Children.Add(Text(notice, caution: true, size: 12));
        if (Str(s, "state") == "held" && s.TryGetProperty("held_deletes", out var hd) && hd.ValueKind == JsonValueKind.Number)
            inner.Children.Add(Row(Text($"{hd.GetInt64()} files would be deleted on the other computer."), Btn("Allow", () => Call("sync_confirm_deletes", new { sync_id = id }))));
        inner.Children.Add(Row(
            Btn("Open in Explorer", () =>
            {
                ShellActions.OpenFile(path);
                return Task.CompletedTask;
            }),
            Btn("Always keep on this PC", () => Call("sync_pin", new { sync_id = id, path = "" })),
            Btn("Free up space", () => Call("sync_unpin", new { sync_id = id, path = "", evict = true }))));
        var verify = Btn("Check again", () => Call("sync_verify", new { sync_id = id }));
        verify.IsEnabled = !paused;
        inner.Children.Add(Row(
            Btn(paused ? "Resume" : "Pause", () => Call(paused ? "sync_resume" : "sync_pause", new { sync_id = id })),
            verify,
            Btn("Leave…", () => LeaveAsync(id, Str(s, "name")))));
        var policy = new ComboBox { Header = "When both sides changed a file", MinWidth = 360 };
        policy.Items.Add(new ComboBoxItem { Content = "the other computer wins, mine is kept as a copy", Tag = "server_wins" });
        policy.Items.Add(new ComboBoxItem { Content = "ask me", Tag = "ask" });
        policy.SelectedIndex = Str(s, "conflict_policy") == "ask" ? 1 : 0;
        policy.SelectionChanged += async (_, _) =>
        {
            if (_building || policy.SelectedItem is not ComboBoxItem { Tag: string tag }) return;
            try { await Call("sync_set", new { sync_id = id, conflict = tag }); }
            catch (Exception ex) { _message = ex.Message; }
            await RefreshAsync();
        };
        inner.Children.Add(policy);
        return Card(inner);
    }

    // ---------- dialogs ----------

    private async Task LeaveAsync(string id, string name)
    {
        var dlg = new ContentDialog
        {
            Title = $"Leave {name}?",
            Content = "The folder stops syncing and its placeholders go. The files stay on the computer that shares them.",
            PrimaryButtonText = "Leave and keep the files I have downloaded",
            SecondaryButtonText = "Leave and delete the files on this PC",
            CloseButtonText = "Cancel",
            XamlRoot = XamlRoot,
        };
        var r = await dlg.ShowAsync();
        if (r == ContentDialogResult.None) return;
        await Call("sync_leave", new { sync_id = id, keep_files = r == ContentDialogResult.Primary });
    }

    private async Task ShareFolderAsync()
    {
        var picker = new FolderPicker { SuggestedStartLocation = PickerLocationId.DocumentsLibrary };
        picker.FileTypeFilter.Add("*");
        WinRT.Interop.InitializeWithWindow.Initialize(picker, OwnerHwnd);
        var folder = await picker.PickSingleFolderAsync();
        if (folder == null) return;
        var name = new TextBox { Header = "Name", Text = folder.Name };
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
        var panel = new StackPanel { Spacing = 12 };
        panel.Children.Add(Text(folder.Path, opacity: 0.7, size: 12));
        panel.Children.Add(name);
        panel.Children.Add(backup);
        panel.Children.Add(Text("The folder stays an ordinary folder. Versions and deleted files are kept inside .medialake-sync next to it.", opacity: 0.7, size: 12));
        var dlg = new ContentDialog { Title = "Share a folder", Content = panel, PrimaryButtonText = "Share", CloseButtonText = "Cancel", XamlRoot = XamlRoot };
        if (await dlg.ShowAsync() != ContentDialogResult.Primary) return;
        var tag = (backup.SelectedItem as ComboBoxItem)?.Tag as string ?? "file:10";
        await Call("sync_host_add", new { name = name.Text.Trim(), root = folder.Path, backup = tag });
    }

    private async Task InviteAsync(List<JsonElement> hosted)
    {
        var panel = new StackPanel { Spacing = 10 };
        panel.Children.Add(Text("Choose what the other computer may use, then give it the code. The code works once.", opacity: 0.7));
        var combos = new List<(string Name, ComboBox Box)>();
        foreach (var h in hosted)
        {
            var box = new ComboBox { Header = Str(h, "name"), MinWidth = 200 };
            foreach (var (label, tag) in new[] { ("No access", "none"), ("Read only", "ro"), ("Read and write", "rw") })
                box.Items.Add(new ComboBoxItem { Content = label, Tag = tag });
            box.SelectedIndex = 0;
            panel.Children.Add(box);
            combos.Add((Str(h, "name"), box));
        }
        var code = new TextBox { IsReadOnly = true, Visibility = Visibility.Collapsed, TextWrapping = TextWrapping.Wrap };
        panel.Children.Add(code);
        var dlg = new ContentDialog { Title = "Invite a device", Content = panel, PrimaryButtonText = "Create code", CloseButtonText = "Close", XamlRoot = XamlRoot };
        dlg.PrimaryButtonClick += async (_, e) =>
        {
            e.Cancel = true; // stays open to show the code
            var grants = combos
                .Select(c => (c.Name, Access: (c.Box.SelectedItem as ComboBoxItem)?.Tag as string ?? "none"))
                .Where(g => g.Access != "none")
                .Select(g => new { sync = g.Name, access = g.Access })
                .ToArray();
            if (grants.Length == 0) return;
            try
            {
                var r = await Call("sync_invite", new { grants });
                code.Text = r.GetProperty("code").GetString() ?? "";
                code.Visibility = Visibility.Visible;
                var pkg = new DataPackage();
                pkg.SetText(code.Text);
                Clipboard.SetContent(pkg); // the code is on the clipboard, ready to send
                dlg.PrimaryButtonText = "Copy again";
            }
            catch (Exception ex)
            {
                code.Text = ex.Message;
                code.Visibility = Visibility.Visible;
            }
        };
        await dlg.ShowAsync();
    }
}
