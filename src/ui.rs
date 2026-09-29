use std::time::Duration;

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Wrap},
};

use crate::{
    app::{App, NodeSort, Overlay, Page, Pane},
    model::Connection,
    subscription::{human_duration, unix_now},
};

const BLUE: Color = Color::Rgb(122, 162, 247);
const CYAN: Color = Color::Rgb(125, 207, 255);
const GREEN: Color = Color::Rgb(158, 206, 106);
const YELLOW: Color = Color::Rgb(224, 175, 104);
const RED: Color = Color::Rgb(247, 118, 142);
const PURPLE: Color = Color::Rgb(187, 154, 247);
const MUTED: Color = Color::Rgb(86, 95, 137);
const TEXT: Color = Color::Rgb(192, 202, 245);
const SURFACE: Color = Color::Rgb(41, 46, 66);
const INK: Color = Color::Rgb(26, 27, 38);

/// Below this width the Proxies page shows only the focused pane.
const SPLIT_MIN_WIDTH: u16 = 70;

pub fn render(frame: &mut Frame<'_>, app: &App) {
    let [top, subscription, content, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_top_bar(frame, app, top);
    render_subscription_bar(frame, app, subscription);
    match app.page {
        Page::Proxies => render_proxies(frame, app, content),
        Page::Connections => render_connections(frame, app, content),
        Page::Rules => render_rules(frame, app, content),
        Page::Logs => render_logs(frame, app, content),
    }
    render_footer(frame, app, footer);

    match app.overlay {
        Overlay::None => {}
        Overlay::Help => render_help(frame, app),
        Overlay::ConfirmCloseAll => render_confirmation(frame, app),
    }
}

fn render_top_bar(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let mut tabs = Vec::new();
    if area.width >= 100 {
        tabs.push(Span::styled(" clash-man ", Style::new().fg(PURPLE).bold()));
    }
    for (index, page) in Page::ALL.into_iter().enumerate() {
        let label = format!(" {} {} ", index + 1, page.title());
        tabs.push(if page == app.page {
            Span::styled(label, Style::new().fg(INK).bg(CYAN).bold())
        } else {
            Span::styled(label, Style::new().fg(MUTED))
        });
    }

    let mut status = if app.connected {
        vec![Span::styled("● online", Style::new().fg(GREEN))]
    } else {
        vec![Span::styled("● offline", Style::new().fg(RED))]
    };
    if !app.config.mode.is_empty() {
        status.push(Span::raw("  "));
        status.push(Span::styled(
            format!(" {} ", capitalize(&app.config.mode)),
            Style::new().fg(INK).bg(mode_color(&app.config.mode)).bold(),
        ));
    }
    if let Some(sample) = app.traffic.back() {
        status.push(Span::styled(
            format!(
                "  ↓ {}/s  ↑ {}/s ",
                compact_bytes(sample.down),
                compact_bytes(sample.up)
            ),
            Style::new().fg(TEXT),
        ));
    }
    render_split_line(frame, area, Line::from(tabs), Line::from(status));
}

fn render_subscription_bar(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let state = &app.subscription;
    let now = unix_now();
    let ago = |time: u64| human_duration(Duration::from_secs(now.saturating_sub(time)));
    let mut left = vec![Span::styled(" Subscription ", Style::new().fg(MUTED))];
    if app.subscription_busy {
        left.push(Span::styled("updating…", Style::new().fg(YELLOW)));
    } else if let Some(error) = &state.last_error {
        left.push(Span::styled(
            format!(
                "✗ failed {} ago: {error}",
                ago(state.last_attempt.unwrap_or(now))
            ),
            Style::new().fg(RED),
        ));
    } else if let Some(success) = state.last_success {
        left.push(Span::styled(
            format!("✓ updated {} ago", ago(success)),
            Style::new().fg(GREEN),
        ));
    } else {
        left.push(Span::styled("never updated", Style::new().fg(YELLOW)));
    }
    if let Some(usage) = &state.usage {
        left.push(Span::styled(
            format!("  {}", usage.summary(now)),
            Style::new().fg(TEXT),
        ));
    }

    let right = Line::from(Span::styled(
        format!(
            "{} conns · mem {} · core {} ",
            app.connections.connections.len(),
            app.memory
                .back()
                .filter(|sample| sample.inuse > 0)
                .map_or_else(|| "—".into(), |sample| human_bytes(sample.inuse)),
            app.version
                .as_ref()
                .map_or("—", |version| version.version.as_str()),
        ),
        Style::new().fg(MUTED),
    ));
    render_split_line(frame, area, Line::from(left), right);
}

/// Draws `left` and right-aligned `right` on one row; `right` is dropped rather than cut when
/// both do not fit.
fn render_split_line(frame: &mut Frame<'_>, area: Rect, left: Line<'_>, right: Line<'_>) {
    let fits = left.width() + 1 + right.width() <= usize::from(area.width);
    let right_width = if fits { right.width() as u16 } else { 0 };
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(right_width)]).areas(area);
    frame.render_widget(Paragraph::new(left), left_area);
    if right_width > 0 {
        frame.render_widget(
            Paragraph::new(right).alignment(Alignment::Right),
            right_area,
        );
    }
}

fn render_proxies(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if area.width < SPLIT_MIN_WIDTH {
        match app.pane {
            Pane::Groups => render_groups(frame, app, area, true),
            Pane::Nodes => render_nodes(frame, app, area, true),
        }
        return;
    }
    let groups_width = (area.width * 32 / 100).clamp(24, 44);
    let [groups, nodes] =
        Layout::horizontal([Constraint::Length(groups_width), Constraint::Fill(1)]).areas(area);
    render_groups(frame, app, groups, app.pane == Pane::Groups);
    render_nodes(frame, app, nodes, app.pane == Pane::Nodes);
}

fn render_groups(frame: &mut Frame<'_>, app: &App, area: Rect, focused: bool) {
    let block = panel(Line::from(" Groups "), focused);
    let names = app.group_names();
    if names.is_empty() {
        render_empty(frame, area, block, waiting_text(app, "No proxy groups"));
        return;
    }
    let name_width = names
        .iter()
        .map(|name| Line::from(*name).width())
        .max()
        .unwrap_or(0)
        .min(16) as u16;
    let (start, end) = window(names.len(), app.group_index, area.height.saturating_sub(2));
    let rows = (start..end).map(|index| {
        let name = names[index];
        let info = &app.proxies.proxies[name];
        let tag = if info.kind.eq_ignore_ascii_case("selector") {
            ""
        } else {
            "auto "
        };
        Row::new([
            Cell::from(name.to_owned()).style(Style::new().fg(TEXT).bold()),
            Cell::from(Line::from(vec![
                Span::styled(tag, Style::new().fg(PURPLE)),
                Span::styled(info.now.clone(), Style::new().fg(MUTED)),
            ])),
        ])
        .style(row_style(index == app.group_index, focused))
    });
    frame.render_widget(
        Table::new(rows, [Constraint::Length(name_width), Constraint::Fill(1)])
            .column_spacing(2)
            .block(block),
        area,
    );
}

fn render_nodes(frame: &mut Frame<'_>, app: &App, area: Rect, focused: bool) {
    let Some((group, info)) = app.selected_group() else {
        render_empty(
            frame,
            area,
            panel(Line::from(" Nodes "), focused),
            waiting_text(app, "Select a group"),
        );
        return;
    };
    let nodes = app.nodes();
    let mut title = vec![
        Span::raw(format!(" {group} ")),
        Span::styled(
            format!("{} · {} nodes ", info.kind, info.all.len()),
            Style::new().fg(MUTED),
        ),
    ];
    if app.node_sort == NodeSort::Latency {
        title.push(Span::styled("· fastest first ", Style::new().fg(MUTED)));
    }
    title.extend(filter_badge(app));
    let mut block = panel(Line::from(title), focused);
    if app.testing.contains(group) {
        block = block
            .title(Line::from(Span::styled(" testing… ", Style::new().fg(YELLOW))).right_aligned());
    }
    if nodes.is_empty() {
        render_empty(frame, area, block, Text::from("No node matches the filter"));
        return;
    }

    let (start, end) = window(nodes.len(), app.node_index, area.height.saturating_sub(2));
    let rows = (start..end).map(|index| {
        let name = nodes[index];
        let active = name == info.now;
        let kind = app
            .proxies
            .proxies
            .get(name)
            .map(|proxy| short_kind(&proxy.kind))
            .unwrap_or_default();
        let delay = app.delay(name);
        Row::new([
            Cell::from(if active { "●" } else { " " }).style(Style::new().fg(GREEN)),
            Cell::from(name.to_owned()).style(if active {
                Style::new().bold()
            } else {
                Style::new()
            }),
            Cell::from(kind).style(Style::new().fg(MUTED)),
            Cell::from(Line::from(delay_text(delay)).right_aligned())
                .style(Style::new().fg(delay_color(delay))),
        ])
        .style(row_style(index == app.node_index, focused))
    });
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(1),
                Constraint::Fill(1),
                Constraint::Length(8),
                Constraint::Length(8),
            ],
        )
        .column_spacing(1)
        .block(block),
        area,
    );
}

fn render_connections(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let connections = app.connections();
    let detail_height = if area.height >= 18 { 6 } else { 0 };
    let [list, detail] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(detail_height)]).areas(area);

    let mut title = vec![
        Span::raw(format!(" Connections · {} ", connections.len())),
        Span::styled(
            format!("· {} first ", app.connection_sort.title()),
            Style::new().fg(MUTED),
        ),
    ];
    title.extend(filter_badge(app));
    let block = panel(Line::from(title), true);
    if connections.is_empty() {
        render_empty(
            frame,
            list,
            block,
            waiting_text(app, "No active connections"),
        );
    } else {
        let wide = area.width >= 100;
        let (start, end) = window(
            connections.len(),
            app.connection_index,
            list.height.saturating_sub(3),
        );
        let rows = (start..end).map(|index| {
            let connection = connections[index];
            let route = route(connection);
            let cells = if wide {
                vec![
                    Cell::from(connection.destination()),
                    Cell::from(connection.metadata.process.clone()).style(Style::new().fg(MUTED)),
                    Cell::from(route),
                    Cell::from(connection.rule.clone()).style(Style::new().fg(MUTED)),
                    Cell::from(Line::from(human_bytes(connection.download)).right_aligned()),
                    Cell::from(Line::from(human_bytes(connection.upload)).right_aligned()),
                ]
            } else {
                vec![
                    Cell::from(connection.destination()),
                    Cell::from(route),
                    Cell::from(Line::from(human_bytes(connection.download)).right_aligned()),
                ]
            };
            Row::new(cells).style(row_style(index == app.connection_index, true))
        });
        let (header, widths) = if wide {
            (
                vec!["Host", "Process", "Route", "Rule", "↓", "↑"],
                vec![
                    Constraint::Fill(3),
                    Constraint::Length(14),
                    Constraint::Fill(2),
                    Constraint::Length(14),
                    Constraint::Length(10),
                    Constraint::Length(10),
                ],
            )
        } else {
            (
                vec!["Host", "Route", "↓"],
                vec![
                    Constraint::Fill(3),
                    Constraint::Fill(2),
                    Constraint::Length(10),
                ],
            )
        };
        frame.render_widget(
            Table::new(rows, widths)
                .header(header_row(header))
                .column_spacing(1)
                .block(block),
            list,
        );
    }

    if detail_height > 0 {
        let text = app
            .selected_connection()
            .map(connection_detail)
            .unwrap_or_default();
        frame.render_widget(
            Paragraph::new(text)
                .block(panel(Line::from(" Details "), false))
                .wrap(Wrap { trim: true }),
            detail,
        );
    }
}

fn render_rules(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let rules = app.rules();
    let mut title = vec![Span::raw(format!(" Rules · {} ", rules.len()))];
    title.extend(filter_badge(app));
    let block = panel(Line::from(title), true);
    if rules.is_empty() {
        render_empty(
            frame,
            area,
            block,
            waiting_text(app, "No rule matches the filter"),
        );
        return;
    }
    let (start, end) = window(rules.len(), app.rule_index, area.height.saturating_sub(3));
    let rows = (start..end).map(|index| {
        let rule = rules[index];
        Row::new([
            Cell::from(rule.kind.clone()).style(Style::new().fg(MUTED)),
            Cell::from(rule.payload.clone()),
            Cell::from(rule.proxy.clone()).style(Style::new().fg(CYAN)),
        ])
        .style(row_style(index == app.rule_index, true))
    });
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(16),
                Constraint::Fill(1),
                Constraint::Length(16),
            ],
        )
        .header(header_row(vec!["Type", "Match", "Policy"]))
        .column_spacing(1)
        .block(block),
        area,
    );
}

fn render_logs(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let logs = app.logs();
    let mut title = vec![
        Span::raw(" Logs "),
        Span::styled(
            format!("· {} and above ", app.log_level.title()),
            Style::new().fg(MUTED),
        ),
        if app.log_follow {
            Span::styled("· following ", Style::new().fg(GREEN))
        } else {
            Span::styled("· paused, G to follow ", Style::new().fg(YELLOW))
        },
    ];
    title.extend(filter_badge(app));
    let block = panel(Line::from(title), true);
    if logs.is_empty() {
        render_empty(
            frame,
            area,
            block,
            Text::from(format!(
                "No {} lines yet · v changes the level",
                app.log_level.title()
            )),
        );
        return;
    }
    let selected = if app.log_follow {
        logs.len().saturating_sub(1)
    } else {
        app.log_index
    };
    let (start, end) = window(logs.len(), selected, area.height.saturating_sub(2));
    let rows = (start..end).map(|index| {
        let log = logs[index];
        Row::new([
            Cell::from(log.level.clone()).style(Style::new().fg(level_color(&log.level))),
            Cell::from(log.payload.clone()),
        ])
        .style(row_style(index == selected, !app.log_follow))
    });
    frame.render_widget(
        Table::new(rows, [Constraint::Length(7), Constraint::Fill(1)])
            .column_spacing(1)
            .block(block),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    if app.editing_filter {
        let left = Line::from(vec![
            Span::styled(" / ", Style::new().fg(CYAN).bold()),
            Span::styled(app.filter.clone(), Style::new().fg(TEXT)),
            Span::styled("▏", Style::new().fg(CYAN)),
        ]);
        let right = hints(&[("Enter", "keep"), ("Esc", "clear"), ("^U", "erase")]);
        render_split_line(frame, area, left, right);
        return;
    }
    let left = Line::from(Span::styled(
        format!(" {}", app.visible_status().unwrap_or_default()),
        Style::new().fg(if app.status_error { RED } else { TEXT }),
    ));
    let keys: &[(&str, &str)] = match (app.page, app.pane) {
        (Page::Proxies, Pane::Groups) => &[
            ("↑↓", "group"),
            ("→/Enter", "nodes"),
            ("t", "test"),
            ("m", "mode"),
            ("u", "update sub"),
            ("?", "help"),
        ],
        (Page::Proxies, Pane::Nodes) => &[
            ("Enter", "use"),
            ("←", "groups"),
            ("t", "test"),
            ("s", "sort"),
            ("/", "filter"),
            ("?", "help"),
        ],
        (Page::Connections, _) => &[
            ("x", "close"),
            ("D", "close all"),
            ("s", "sort"),
            ("/", "filter"),
            ("?", "help"),
        ],
        (Page::Rules, _) => &[("/", "filter"), ("g/G", "top/bottom"), ("?", "help")],
        (Page::Logs, _) => &[
            ("v", "level"),
            ("G", "follow"),
            ("c", "clear"),
            ("/", "filter"),
            ("?", "help"),
        ],
    };
    render_split_line(frame, area, left, hints(keys));
}

fn render_help(frame: &mut Frame<'_>, app: &App) {
    type Section = (&'static str, &'static [(&'static str, &'static str)]);
    const LEFT: [Section; 2] = [
        (
            "Everywhere",
            &[
                ("1-4 Tab", "switch page"),
                ("↑↓ j k", "move"),
                ("PgUp PgDn", "move a page"),
                ("g G", "top / bottom"),
                ("/", "filter, Esc clears"),
                ("m", "mode rule/global/direct"),
                ("u", "update subscription"),
                ("r", "refresh everything"),
                ("q", "quit"),
            ],
        ),
        ("Logs", &[("v", "minimum level"), ("c", "clear")]),
    ];
    const RIGHT: [Section; 2] = [
        (
            "Proxies",
            &[
                ("← → h l", "groups / nodes"),
                ("Enter", "open group / use node"),
                ("t", "test latency"),
                ("s", "fastest first"),
            ],
        ),
        (
            "Connections",
            &[("x", "close one"), ("D", "close all"), ("s", "cycle sort")],
        ),
    ];
    let column = |sections: &[Section]| {
        let mut lines = Vec::new();
        for (title, keys) in sections {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(Line::styled(*title, Style::new().fg(PURPLE).bold()));
            for (key, action) in *keys {
                lines.push(Line::from(vec![
                    Span::styled(format!(" {key:<10}"), Style::new().fg(CYAN)),
                    Span::styled(*action, Style::new().fg(TEXT)),
                ]));
            }
        }
        lines
    };
    let (left, right) = (column(&LEFT), column(&RIGHT));
    let config = &app.config;
    let info = vec![
        Line::styled(
            app.config_source
                .as_deref()
                .unwrap_or("config not discovered")
                .to_owned(),
            Style::new().fg(MUTED),
        ),
        Line::styled(
            format!(
                "http {} · socks {} · mixed {} · allow-lan {} · tun {}",
                config.port,
                config.socks_port,
                config.mixed_port,
                config.allow_lan,
                config.tun.enable
            ),
            Style::new().fg(MUTED),
        ),
    ];

    let keys_height = left.len().max(right.len()) as u16;
    let area = popup(frame, 78, keys_height + info.len() as u16 + 3);
    let block = panel(Line::from(" Keys · any key closes "), true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [keys, _, info_area] = Layout::vertical([
        Constraint::Length(keys_height),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(inner);
    let [left_area, right_area] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)]).areas(keys);
    frame.render_widget(Paragraph::new(left), left_area);
    frame.render_widget(Paragraph::new(right), right_area);
    frame.render_widget(Paragraph::new(info), info_area);
}

fn render_confirmation(frame: &mut Frame<'_>, app: &App) {
    let area = popup(frame, 48, 4);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!(
                "Close all {} connections?",
                app.connections.connections.len()
            )),
            Line::from(vec![
                Span::styled("y", Style::new().fg(RED).bold()),
                Span::styled(" confirm · any other key cancels", Style::new().fg(MUTED)),
            ]),
        ])
        .alignment(Alignment::Center)
        .block(panel(Line::from(" Confirm "), true)),
        area,
    );
}

fn connection_detail(connection: &Connection) -> Text<'static> {
    let metadata = &connection.metadata;
    Text::from(vec![
        Line::from(vec![
            label("From"),
            Span::raw(format!("{}:{}", metadata.source_ip, metadata.source_port)),
            label("  Network"),
            Span::raw(format!("{} {}", metadata.network, metadata.kind)),
            label("  Started"),
            Span::raw(clock_time(&connection.start)),
        ]),
        Line::from(vec![
            label("Process"),
            Span::raw(metadata.process_path.clone()),
        ]),
        Line::from(vec![
            label("Rule"),
            Span::raw(format!("{} {}", connection.rule, connection.rule_payload)),
        ]),
        Line::from(vec![label("Route"), Span::raw(route(connection))]),
    ])
}

/// Group path from the matched policy to the node, e.g. `Others › Proxy › 日本 04`.
fn route(connection: &Connection) -> String {
    connection
        .chains
        .iter()
        .rev()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" › ")
}

/// `2026-09-28T18:40:12.1234+08:00` → `18:40:12`.
fn clock_time(timestamp: &str) -> String {
    timestamp
        .split_once('T')
        .map(|(_, time)| time.chars().take(8).collect())
        .unwrap_or_else(|| timestamp.to_owned())
}

fn render_empty(frame: &mut Frame<'_>, area: Rect, block: Block<'_>, text: Text<'_>) {
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [middle] = Layout::vertical([Constraint::Length(text.height() as u16)])
        .flex(ratatui::layout::Flex::Center)
        .areas(inner);
    frame.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .style(Style::new().fg(MUTED)),
        middle,
    );
}

fn waiting_text(app: &App, empty: &'static str) -> Text<'static> {
    Text::from(if app.connected {
        empty
    } else {
        "Waiting for the controller…"
    })
}

fn filter_badge(app: &App) -> Option<Span<'static>> {
    (!app.filter.is_empty()).then(|| {
        Span::styled(
            format!(" /{} ", app.filter),
            Style::new().fg(INK).bg(YELLOW),
        )
    })
}

fn hints(keys: &[(&str, &str)]) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, (key, action)) in keys.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ", Style::new()));
        }
        spans.push(Span::styled(key.to_string(), Style::new().fg(CYAN)));
        spans.push(Span::styled(format!(" {action}"), Style::new().fg(MUTED)));
    }
    spans.push(Span::raw(" "));
    Line::from(spans)
}

fn header_row(titles: Vec<&'static str>) -> Row<'static> {
    Row::new(titles).style(Style::new().fg(BLUE).bold())
}

fn panel<'a>(title: Line<'a>, focused: bool) -> Block<'a> {
    let (border, title_style) = if focused {
        (Style::new().fg(BLUE), Style::new().fg(TEXT).bold())
    } else {
        (Style::new().fg(SURFACE), Style::new().fg(MUTED))
    };
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(border)
        .title(title)
        .title_style(title_style)
}

/// The cursor row is bright in the focused pane and dim elsewhere, so focus is always visible.
fn row_style(selected: bool, focused: bool) -> Style {
    match (selected, focused) {
        (true, true) => Style::new().fg(INK).bg(BLUE).add_modifier(Modifier::BOLD),
        (true, false) => Style::new().fg(TEXT).bg(SURFACE),
        (false, _) => Style::new().fg(TEXT),
    }
}

fn label(text: &str) -> Span<'static> {
    Span::styled(format!("{text} "), Style::new().fg(MUTED))
}

fn delay_text(delay: Option<u64>) -> String {
    match delay {
        None => "—".into(),
        Some(0) => "timeout".into(),
        Some(delay) => format!("{delay} ms"),
    }
}

fn delay_color(delay: Option<u64>) -> Color {
    match delay {
        None => MUTED,
        Some(1..=150) => GREEN,
        Some(151..=300) => YELLOW,
        Some(_) => RED,
    }
}

fn level_color(level: &str) -> Color {
    match level.to_ascii_lowercase().as_str() {
        "error" | "fatal" => RED,
        "warning" | "warn" => YELLOW,
        "debug" => MUTED,
        _ => CYAN,
    }
}

fn mode_color(mode: &str) -> Color {
    match mode.to_ascii_lowercase().as_str() {
        "rule" => GREEN,
        "global" => YELLOW,
        _ => RED,
    }
}

fn short_kind(kind: &str) -> String {
    match kind.to_ascii_lowercase().as_str() {
        "shadowsocks" => "ss".into(),
        "shadowsocksr" => "ssr".into(),
        "urltest" => "auto".into(),
        other => other.into(),
    }
}

fn capitalize(text: &str) -> String {
    let mut characters = text.chars();
    characters
        .next()
        .map(|first| first.to_uppercase().chain(characters).collect())
        .unwrap_or_default()
}

/// Rows `[start, end)` to draw so that `selected` stays near the middle of `height` rows.
fn window(total: usize, selected: usize, height: u16) -> (usize, usize) {
    let visible = usize::from(height).max(1);
    let start = selected
        .saturating_sub(visible / 2)
        .min(total.saturating_sub(visible));
    (start, (start + visible).min(total))
}

/// Clears the full width of the popup's rows, so no double-width character straddles its
/// border, and returns the centered popup area.
fn popup(frame: &mut Frame<'_>, width: u16, height: u16) -> Rect {
    let area = centered(frame.area(), width, height);
    frame.render_widget(
        Clear,
        Rect {
            x: 0,
            width: frame.area().width,
            ..area
        },
    );
    area
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

/// `10.4K`, `1.2M`: narrow enough for the top bar.
fn compact_bytes(value: u64) -> String {
    let full = human_bytes(value);
    full.replace(" KiB", "K")
        .replace(" MiB", "M")
        .replace(" GiB", "G")
        .replace(" TiB", "T")
}

pub fn human_bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut amount = value as f64;
    let mut unit = 0;
    while amount >= 1024.0 && unit < UNITS.len() - 1 {
        amount /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", value, UNITS[unit])
    } else {
        format!("{amount:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::{human_bytes, render, window};
    use crate::app::{ApiEvent, App};
    use crate::model::{ProxyInfo, ProxyPayload};

    fn draw(app: &App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        terminal.backend().to_string()
    }

    fn app_with_group() -> App {
        let mut app = App::new(Default::default());
        let mut payload = ProxyPayload::default();
        payload.proxies.insert(
            "Proxy".into(),
            ProxyInfo {
                kind: "Selector".into(),
                now: "日本 04".into(),
                all: vec!["台湾 01".into(), "日本 04".into()],
                ..Default::default()
            },
        );
        app.apply(ApiEvent::Proxies(Ok(payload)));
        app
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(human_bytes(1_536), "1.5 KiB");
    }

    #[test]
    fn keeps_cursor_near_the_middle() {
        assert_eq!(window(100, 50, 10), (45, 55));
        assert_eq!(window(100, 2, 10), (0, 10));
        assert_eq!(window(100, 99, 10), (90, 100));
        assert_eq!(window(3, 1, 10), (0, 3));
    }

    #[test]
    fn proxies_page_shows_groups_nodes_and_active_marker() {
        let output = draw(&app_with_group(), 100, 20);
        assert!(output.contains("1 Proxies"));
        assert!(output.contains("Groups"));
        assert!(output.contains("● 日本 04"));
        assert!(output.contains("Subscription"));
    }

    #[test]
    fn narrow_terminal_shows_only_the_focused_pane() {
        let output = draw(&app_with_group(), 50, 14);
        assert!(output.contains("Groups"));
        assert!(!output.contains("Selector · 2 nodes"));
    }

    #[test]
    fn every_page_renders_empty_without_panicking() {
        let mut app = App::new(Default::default());
        for page in crate::app::Page::ALL {
            app.page = page;
            for (width, height) in [(120, 40), (80, 24), (40, 10), (10, 4)] {
                draw(&app, width, height);
            }
        }
    }
}
