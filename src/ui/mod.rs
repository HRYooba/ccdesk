//! サイドバー・右ペインの描画と、描画／クリック判定で共有するジオメトリ計算。
pub(crate) mod new_view;

use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem};
use ratatui::Frame;
use std::time::Duration;
use tui_term::widget::PseudoTerminal;

use crate::app::{App, Focus, Popup, RightView, RowAction, SIDEBAR_HEADER_ROWS};
use crate::poll::{classify, Bucket, Group, Grouping, StateView};
use crate::session::SessionStatus;
use crate::theme::{
    ui, usage_color, C_ATTENTION, C_FAIL, C_WORKING, FOCUS_BORDER, MUTED_FG,
};
use crate::ui::new_view::draw_new_view;

/// リセット時刻のローカル表記。当日なら "14:00"、別日なら "7/29 09:00"
fn fmt_reset_at(resets_at: u64) -> String {
    use chrono::{Datelike, Local, TimeZone, Timelike};
    let Some(t) = Local.timestamp_opt(resets_at as i64, 0).single() else {
        return String::new();
    };
    let today = Local::now().date_naive();
    if t.date_naive() == today {
        format!("{:02}:{:02}", t.hour(), t.minute())
    } else {
        format!("{}/{} {:02}:{:02}", t.month(), t.day(), t.hour(), t.minute())
    }
}

/// サイドバーのジオメトリ（描画とクリック判定で同じ計算を共有する）
pub(crate) struct SidebarLayout {
    /// 一覧に使える行数（枠とフッターを除く）
    pub(crate) capacity: usize,
    /// フッターを描くか（狭すぎる端末では描かない = クリックも受けない）
    pub(crate) footer_visible: bool,
    /// 更新ボタン行を出すか
    pub(crate) update_row_visible: bool,
    /// アカウント行の画面 y（footer_visible のときだけ有効）
    pub(crate) account_y: u16,
}

pub(crate) fn sidebar_layout(app: &App) -> SidebarLayout {
    // 下部バー 1 行を除いたサイドバー矩形は draw の chunks[0] と一致する
    let height = app.term_size.1.saturating_sub(1);
    let updating = app
        .claude_updating
        .load(std::sync::atomic::Ordering::Relaxed);
    let update_row_visible = app.footer.latest.is_some() || updating;
    let footer_visible = height >= 8 && app.sidebar_width > 4;
    let footer_rows = if footer_visible {
        2 + usize::from(update_row_visible)
    } else {
        0
    };
    SidebarLayout {
        capacity: (height as usize).saturating_sub(2 + footer_rows),
        footer_visible,
        update_row_visible,
        account_y: height.saturating_sub(2),
    }
}

/// モーダルの矩形。描画とクリック判定で同じ計算を共有する
pub(crate) fn popup_rect(app: &App, popup: &Popup) -> Rect {
    let entries = popup.entries(app.grouping);
    let width = 14u16;
    let height = entries.len() as u16 + 2;
    let x = 1u16.min(app.sidebar_width.saturating_sub(width));
    let y = (popup.anchor_y + 1).min(app.term_size.1.saturating_sub(height));
    Rect::new(x, y, width, height)
}

fn fmt_age(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

pub(crate) fn draw(frame: &mut Frame, app: &mut App) {
    // 最下行は横断のキーヒントバー
    let vert = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(frame.area());
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(app.sidebar_width),
            Constraint::Min(1),
        ])
        .split(vert[0]);

    // 下部バー: 通知（attach 失敗等）があれば数秒それを出し、無ければキーヒント
    if let Some((msg, at)) = &app.notice {
        if at.elapsed() < Duration::from_secs(5) {
            frame.render_widget(
                ratatui::widgets::Paragraph::new(Line::from(format!(" {msg}")))
                    .style(Style::default().fg(C_FAIL)),
                vert[1],
            );
        } else {
            app.notice = None;
        }
    }
    if app.notice.is_none() {
        let mut hint_spans = vec![
            Span::styled(" app:", Style::default().fg(MUTED_FG)),
            Span::raw(" Ctrl+Q quit · Alt+←→ focus"),
        ];
        if app.focus == Focus::Sidebar {
            hint_spans.push(Span::styled("  sidebar:", Style::default().fg(MUTED_FG)));
            hint_spans.push(Span::raw(
                " ↑↓ select · Enter open · Ctrl+S group · Ctrl+X stop→delete",
            ));
        }
        // 右端: 5h/7d 使用率とリセット時刻（opt-in。statusline フック由来の公式データ）。
        // 古いデータ（10 分超更新なし）は消さず、全体を dim に落として区別する
        let mut usage_spans: Vec<Span> = Vec::new();
        if let Some(usage) = &app.usage {
            let stale = usage.stale;
            let ring = |pct: f64| ["○", "◔", "◑", "◕", "●"][(pct / 25.0).min(4.0) as usize];
            let mut push_window = |label: &str, w: Option<(f64, u64)>| {
                if let Some((pct, resets)) = w {
                    if !usage_spans.is_empty() {
                        usage_spans.push(Span::styled(" · ", Style::default().fg(ui().dim)));
                    }
                    usage_spans.push(Span::styled(
                        format!("{label} "),
                        Style::default().fg(ui().dim),
                    ));
                    let value_color = if stale { ui().dim } else { usage_color(pct) };
                    usage_spans.push(Span::styled(
                        format!("{} {}%", ring(pct), pct.round() as u32),
                        Style::default().fg(value_color),
                    ));
                    if resets > 0 {
                        usage_spans.push(Span::styled(
                            format!(" →{}", fmt_reset_at(resets)),
                            Style::default().fg(ui().dim),
                        ));
                    }
                }
            };
            push_window("5h", usage.five);
            push_window("7d", usage.seven);
            usage_spans.push(Span::raw(" "));
        }
        let usage_w = usage_spans
            .iter()
            .map(|s| s.content.chars().count() as u16)
            .sum::<u16>();
        let bar = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(usage_w)])
            .split(vert[1]);
        // new session 画面のヒントはペイン内に出すため、下部バーには重ねない
        frame.render_widget(
            ratatui::widgets::Paragraph::new(Line::from(hint_spans))
                .style(Style::default().fg(ui().dim)),
            bar[0],
        );
        if usage_w > 0 {
            frame.render_widget(
                ratatui::widgets::Paragraph::new(Line::from(usage_spans)),
                bar[1],
            );
        }
    }

    // サイドバー: 行の正本は agents --json（ライブ）+ state.json（summary 補完）。
    // 自分の PTY 行は「attach ウィンドウ」としてだけ出す
    let active = app.active;
    let home = std::env::var("USERPROFILE").unwrap_or_default();
    let shorten = |p: &str| {
        if !home.is_empty() && p.starts_with(&home) {
            format!("~{}", &p[home.len()..])
        } else {
            p.to_string()
        }
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    // 自分のウィンドウの表示は agents --json（attach_id で突合）→ classify で決める。
    // agents に居なければ出力ヒューリスティックへフォールバック
    let agents_snapshot = app.agents.clone();
    let views: Vec<StateView> = app
        .sessions
        .iter_mut()
        .map(|s| {
            if let Some(agent) = s
                .attach_id
                .as_deref()
                .and_then(|id| agents_snapshot.iter().find(|a| a.id == id))
            {
                classify(&agent.state, false, agent.has_pid)
            } else if !s.alive() {
                classify("stopped", false, false)
            } else {
                match s.status_heuristic() {
                    SessionStatus::Working => classify("working", false, true),
                    SessionStatus::NeedsInput => classify("blocked", false, true),
                    SessionStatus::Exited => classify("stopped", false, false),
                }
            }
        })
        .collect();

    // ---- 行データを先に組み立てる（State / Directory 両グルーピング対応）----
    struct RowData {
        action: RowAction,
        group: Group,
        cwd: String,
        glyph: &'static str,
        color: Color,
        label: String,
        is_active_window: bool,
        status_label: &'static str,
        summary: String,
        children: Vec<String>,
        age: String,
        bucket: Bucket,
    }
    // 公式の Working スピナーは点滅アニメ
    let spinner = if (now_ms / 400).is_multiple_of(2) { "✽" } else { "✻" };
    // 公式準拠: 形状 = プロセス生死（✻ 生存 / ∙ 終了）、Working は点滅
    let glyph_of = |view: &StateView| -> &'static str {
        if view.spinning {
            spinner
        } else if view.alive {
            "✻"
        } else {
            "∙"
        }
    };

    let mut data: Vec<RowData> = Vec::new();
    for (i, session) in app.sessions.iter().enumerate() {
        let view = views[i];
        let age_secs = session
            .last_output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
            .as_secs();
        data.push(RowData {
            action: RowAction::Open(session.attach_id.clone().unwrap_or_default()),
            group: view.group,
            cwd: session.cwd.clone(),
            glyph: glyph_of(&view),
            color: view.color,
            label: session.name.clone(),
            is_active_window: i == active && matches!(app.right_view, RightView::Sessions),
            status_label: view.label,
            summary: String::new(),
            children: Vec::new(),
            age: fmt_age(age_secs),
            bucket: view.bucket,
        });
    }
    for job in app.jobs.iter() {
        if app
            .sessions
            .iter()
            .any(|s| s.attach_id.as_deref() == Some(job.short.as_str()))
        {
            continue; // attach 中は自分のウィンドウ行が代表する（集計もウィンドウ行が担う）
        }
        // agents --json のライブ状態を優先（rename・state 変化が即時反映される）
        let agent = app.agents.iter().find(|a| a.id == job.short);
        let live_state = agent
            .map(|a| a.state.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(job.state.as_str());
        let alive = agent.map(|a| a.has_pid).unwrap_or(false);
        let view = classify(live_state, job.tempo == "blocked", alive);
        let summary = match view.label {
            "Done" | "Failed" => job.result.clone(),
            "Needs input" => job.needs.clone(),
            _ => job.detail.clone(),
        };
        // 表示名はライブ名を優先。未命名はプロンプト投入前なら "new session"、他は "bg"
        let live_name = agent
            .map(|a| a.name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| job.name.clone());
        let label = if !live_name.is_empty() {
            live_name
        } else if live_state == "working" && job.result.is_empty() {
            "new session".to_string()
        } else {
            "bg".to_string()
        };
        // 公式の経過時間: 作業中 = 作成からの経過、終了後 = 総実行時間で凍結。
        // 隣に出す状態表示と同じ live_state で判定する（state.json 直読みは古い）
        let age_secs = match live_state {
            "done" | "failed" | "stopped" if job.updated_at_ms >= job.created_at_ms => {
                (job.updated_at_ms - job.created_at_ms) / 1000
            }
            _ if job.created_at_ms > 0 => now_ms.saturating_sub(job.created_at_ms) / 1000,
            _ => job.mtime.elapsed().map(|d| d.as_secs()).unwrap_or(0),
        };
        // PR 持ちで未完了なら Ready for review（公式: 完了系はそのまま Completed へ）
        let group = if !job.children.is_empty() && view.group != Group::Completed {
            Group::ReadyForReview
        } else {
            view.group
        };
        data.push(RowData {
            action: RowAction::Open(job.short.clone()),
            group,
            cwd: job.cwd.clone(),
            glyph: glyph_of(&view),
            color: view.color,
            label,
            is_active_window: false,
            status_label: view.label,
            summary,
            children: job.children.clone(),
            age: fmt_age(age_secs),
            bucket: view.bucket,
        });
    }
    // ヘッダー集計は表示行そのものから数える（分岐の複製をしない = 行数と必ず一致）
    let mut awaiting = 0usize;
    let mut working = 0usize;
    let mut completed = 0usize;
    for d in &data {
        match d.bucket {
            Bucket::Awaiting => awaiting += 1,
            Bucket::Working => working += 1,
            Bucket::Completed => completed += 1,
        }
    }

    // ---- 描画 ----
    let hovered = app.hovered_row;
    let selected = app.selected_row;
    let mut items: Vec<ListItem> = Vec::new();
    let mut rows: Vec<Option<RowAction>> = Vec::new();

    let push_data_row =
        |items: &mut Vec<ListItem>, rows: &mut Vec<Option<RowAction>>, d: &RowData| {
            let cur = rows.len();
            let highlighted = hovered == Some(cur) || selected == cur;
            let mut line_style = Style::default();
            if highlighted {
                line_style = line_style.bg(ui().hl_bg).fg(ui().emph);
            }
            let name_style = if d.is_active_window {
                Style::default().fg(ui().emph).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let mut spans = vec![
                Span::styled(
                    "☰",
                    Style::default().fg(if highlighted {
                        ui().emph
                    } else {
                        MUTED_FG
                    }),
                ),
                Span::raw(" "),
                Span::styled(d.glyph, Style::default().fg(d.color)),
                Span::raw(" "),
                Span::styled(d.label.clone(), name_style),
                Span::raw("  "),
                Span::styled(d.status_label, Style::default().fg(d.color)),
            ];
            if !d.summary.is_empty() {
                spans.push(Span::raw(" · "));
                spans.push(Span::raw(d.summary.clone()));
            }
            for child in &d.children {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(child.clone(), Style::default().fg(C_ATTENTION)));
            }
            spans.push(Span::raw(" · "));
            spans.push(Span::styled(
                d.age.clone(),
                Style::default().fg(ui().dim),
            ));
            items.push(ListItem::new(Line::from(spans).style(line_style)));
            rows.push(Some(d.action.clone()));
        };

    // 先頭: 新規セッション
    {
        let cur = rows.len();
        let highlighted = hovered == Some(cur) || selected == cur;
        let mut style = Style::default();
        if highlighted {
            style = style.bg(ui().hl_bg).fg(ui().emph);
        }
        items.push(ListItem::new(Line::from("+ new session").style(style)));
        rows.push(Some(RowAction::New));
    }
    // 区切り線: new session（アクション）とセッション一覧領域を分ける（Desktop 風）
    items.push(ListItem::new(
        Line::from("─".repeat(chunks[0].width.saturating_sub(2) as usize))
            .style(Style::default().fg(ui().dim)),
    ));
    rows.push(None);
    // グルーピング切替（クリックで state ⇔ directory）
    {
        let cur = rows.len();
        let highlighted = hovered == Some(cur) || selected == cur;
        let mut style = Style::default().fg(ui().dim);
        if highlighted {
            style = style.bg(ui().hl_bg).fg(ui().emph);
        }
        let chosen = if app.grouping == Grouping::State {
            "state"
        } else {
            "directory"
        };
        items.push(ListItem::new(
            Line::from(vec![
                Span::raw("⊞ group: "),
                Span::styled(chosen, Style::default().fg(ui().emph)),
            ])
            .style(style),
        ));
        rows.push(Some(RowAction::ToggleGroup));
    }
    // ヘッダー集計行（公式ヘッダー相当）
    items.push(ListItem::new(
        Line::from(format!(
            "{awaiting} awaiting input · {working} working · {completed} completed"
        ))
        .style(Style::default().fg(ui().dim)),
    ));
    rows.push(None);

    match app.grouping {
        Grouping::State => {
            for group in [
                Group::ReadyForReview,
                Group::NeedsInput,
                Group::Working,
                Group::Completed,
            ] {
                let members: Vec<&RowData> =
                    data.iter().filter(|d| d.group == group).collect();
                if members.is_empty() {
                    continue;
                }
                items.push(ListItem::new(Line::from("")));
                rows.push(None);
                items.push(ListItem::new(
                    Line::from(group.title()).style(Style::default().fg(ui().dim)),
                ));
                rows.push(None);
                for d in members {
                    push_data_row(&mut items, &mut rows, d);
                }
            }
        }
        Grouping::Directory => {
            // ディレクトリの並びはプロジェクト名のアルファベット順で固定。
            // 選択・stop・delete 等の操作では並び替えない
            let mut cwds: Vec<&str> = Vec::new();
            for j in &app.jobs {
                if !cwds.contains(&j.cwd.as_str()) {
                    cwds.push(&j.cwd);
                }
            }
            for d in &data {
                if !cwds.contains(&d.cwd.as_str()) {
                    cwds.push(&d.cwd);
                }
            }
            cwds.sort_by_key(|c| {
                std::path::Path::new(c)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_lowercase())
                    .unwrap_or_default()
            });
            for cwd in cwds {
                items.push(ListItem::new(Line::from("")));
                rows.push(None);
                // 見出し = プロジェクト名（末端ディレクトリ名）+ このフォルダで新規を開く +
                let leaf = std::path::Path::new(cwd)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| shorten(cwd));
                let cur = rows.len();
                let highlighted = hovered == Some(cur) || selected == cur;
                let mut style = Style::default().fg(ui().dim);
                if highlighted {
                    style = style.bg(ui().hl_bg).fg(ui().emph);
                }
                items.push(ListItem::new(
                    Line::from(vec![Span::raw(leaf), Span::raw("  +")]).style(style),
                ));
                rows.push(Some(RowAction::NewIn(cwd.to_string())));
                for d in data.iter().filter(|d| d.cwd == cwd) {
                    push_data_row(&mut items, &mut rows, d);
                }
            }
        }
    }

    // 下部のフッター（区切り線 + 更新ボタン行 + アカウント行）を差し引いた行数が表示窓。
    // 溢れる分はスクロールで届く（ホイール / ↑↓ の選択追従）。
    // ジオメトリはクリック判定と同じ sidebar_layout を使う
    let sl = sidebar_layout(app);
    let updating = app
        .claude_updating
        .load(std::sync::atomic::Ordering::Relaxed);
    let capacity = sl.capacity;
    app.sidebar_rows = rows;
    // 行構成が変わって選択が浮いたら、最寄りのクリック可能行へ寄せる
    if app
        .sidebar_rows
        .get(app.selected_row)
        .map(|r| r.is_none())
        .unwrap_or(true)
    {
        app.selected_row = app
            .sidebar_rows
            .iter()
            .position(|r| r.is_some())
            .unwrap_or(0);
    }

    // ヘッダー行は固定表示。スクロールはその下（セッション一覧）にだけ効く。
    // ↑↓ 直後だけ選択行へ追従し、常に範囲内へクランプ
    let header_n = SIDEBAR_HEADER_ROWS.min(items.len());
    let tail_capacity = capacity.saturating_sub(header_n);
    if app.sidebar_follow_sel {
        app.sidebar_follow_sel = false;
        if app.selected_row >= header_n {
            let sel_t = app.selected_row - header_n;
            if sel_t < app.sidebar_scroll {
                app.sidebar_scroll = sel_t;
            } else if tail_capacity > 0 && sel_t >= app.sidebar_scroll + tail_capacity {
                app.sidebar_scroll = sel_t + 1 - tail_capacity;
            }
        }
    }
    app.sidebar_scroll = app
        .sidebar_scroll
        .min((items.len() - header_n).saturating_sub(tail_capacity));

    // フォーカス中のペインだけ枠を少し明るく
    let focus_style = Style::default().fg(FOCUS_BORDER);
    let blur_style = Style::default().fg(ui().dim);
    let scroll = app.sidebar_scroll;
    let visible: Vec<ListItem> = items
        .into_iter()
        .enumerate()
        .filter(|(i, _)| {
            *i < header_n
                || (*i >= header_n + scroll && *i < header_n + scroll + tail_capacity)
        })
        .map(|(_, item)| item)
        .collect();
    let list = List::new(visible).block(Block::default().borders(Borders::ALL).border_style(
        if app.focus == Focus::Sidebar {
            focus_style
        } else {
            blur_style
        },
    ));
    frame.render_widget(list, chunks[0]);

    // ---- サイドバー下部フッター: 区切り線 / 更新ボタン行（差分時のみ）/ アカウント行 ----
    if sl.footer_visible {
        let update_row_visible = sl.update_row_visible;
        let fx = chunks[0].x + 1;
        let fw = chunks[0].width - 2;
        let account_y = sl.account_y;
        let sep_y = account_y - 1 - u16::from(update_row_visible);
        let clip = |s: &str| -> String { s.chars().take(fw as usize).collect() };
        // 区切り線（Desktop 風にフッターを本文から分ける）
        frame.render_widget(
            ratatui::widgets::Paragraph::new(
                Line::from("─".repeat(fw as usize)).style(Style::default().fg(ui().dim)),
            ),
            Rect::new(fx, sep_y, fw, 1),
        );
        // アカウント行（表示名 · 組織名）
        frame.render_widget(
            ratatui::widgets::Paragraph::new(
                Line::from(clip(&app.footer.account)).style(Style::default().fg(ui().dim)),
            ),
            Rect::new(fx, account_y, fw, 1),
        );
        if update_row_visible {
            let label = if updating {
                "⟳ updating claude…".to_string()
            } else {
                format!(
                    "⟳ update claude {} → {}",
                    app.footer.current,
                    app.footer.latest.as_deref().unwrap_or("")
                )
            };
            let style = if updating {
                Style::default().fg(C_WORKING)
            } else {
                Style::default().fg(MUTED_FG)
            };
            frame.render_widget(
                ratatui::widgets::Paragraph::new(Line::from(clip(&label)).style(style)),
                Rect::new(fx, account_y - 1, fw, 1),
            );
        }
    }

    // コンテキストメニュー（モーダル）。矩形はクリック判定と同じ popup_rect を使う
    if let Some(popup) = &app.popup {
        let entries = popup.entries(app.grouping);
        let area = popup_rect(app, popup);
        frame.render_widget(ratatui::widgets::Clear, area);
        let lines: Vec<ListItem> = entries
            .iter()
            .enumerate()
            .map(|(i, (label, enabled))| {
                let mut style = if *enabled {
                    Style::default()
                } else {
                    Style::default().fg(ui().dim)
                };
                if i == popup.selected {
                    style = style.bg(ui().hl_bg);
                    if *enabled {
                        style = style.fg(ui().emph);
                    }
                }
                ListItem::new(Line::from(format!(" {label}")).style(style))
            })
            .collect();
        frame.render_widget(
            List::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(ui().dim)),
            ),
            area,
        );
    }

    // 右ペイン: 新規セッション画面 or アクティブセッションの画面
    let terminal_focused = app.focus == Focus::Terminal;
    let starting = app.spawn_rx.is_some();
    if let RightView::New(state) = &mut app.right_view {
        draw_new_view(frame, chunks[1], state, terminal_focused, starting);
        return;
    }
    if app.sessions.is_empty() {
        frame.render_widget(
            Block::default()
                .borders(Borders::ALL)
                .title("no session")
                .border_style(Style::default().fg(ui().dim)),
            chunks[1],
        );
        return;
    }
    let session = &app.sessions[app.active];
    let parser = session.parser.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let screen = parser.screen();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(session.name.clone())
        .border_style(if app.focus == Focus::Terminal {
            focus_style
        } else {
            blur_style
        });
    let inner = block.inner(chunks[1]);
    // tui-term 独自の █ カーソル描画は無効化し、ネイティブカーソル
    // （set_cursor_position = 本家と同じ点滅バー）だけを使う
    let widget = PseudoTerminal::new(screen)
        .cursor(tui_term::widget::Cursor::default().visibility(false))
        .block(block);
    frame.render_widget(widget, chunks[1]);

    // カーソル位置を反映（フォーカス外は隠す。ペイン外へはみ出す座標はクランプ）
    if app.focus == Focus::Terminal && !screen.hide_cursor() {
        let (crow, ccol) = screen.cursor_position();
        if ccol < inner.width && crow < inner.height {
            frame.set_cursor_position(Position::new(inner.x + ccol, inner.y + crow));
        }
    }
}
