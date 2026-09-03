mod tokens;

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::Span,
};

pub(crate) use self::tokens::{
    agent_rows as sidebar_agent_rows, space_rows as sidebar_space_rows, AgentTokenContext,
    ResolvedToken, ResolvedTokenKind, SpaceTokenContext,
};
use super::text::{display_width, truncate_end};
use crate::app::state::Palette;
use crate::app::AppState;
use crate::detect::AgentState;
use crate::terminal::TerminalRuntimeRegistry;

pub(crate) struct AgentPanelEntry {
    pub ws_idx: usize,
    pub tab_idx: usize,
    pub pane_id: crate::layout::PaneId,
    pub agent_kind_label: Option<String>,
    pub state: AgentState,
    pub seen: bool,
    pub last_agent_state_change_seq: Option<u64>,
    pub tokens: std::collections::HashMap<String, String>,
}

fn sidebar_section_heights(total_height: u16, split_ratio: f32) -> (u16, u16) {
    if total_height == 0 {
        return (0, 0);
    }
    if total_height < 6 {
        let workspace_height = total_height.div_ceil(2);
        return (
            workspace_height,
            total_height.saturating_sub(workspace_height),
        );
    }

    let workspace_height = ((total_height as f32) * split_ratio.clamp(0.1, 0.9)).round() as u16;
    let workspace_height = workspace_height.clamp(3, total_height.saturating_sub(3));
    (
        workspace_height,
        total_height.saturating_sub(workspace_height),
    )
}

pub(crate) fn expanded_sidebar_sections(area: Rect, split_ratio: f32) -> (Rect, Rect) {
    let content = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
    if content.is_empty() {
        return (Rect::default(), Rect::default());
    }

    let (workspace_height, detail_height) = sidebar_section_heights(content.height, split_ratio);
    (
        Rect::new(content.x, content.y, content.width, workspace_height),
        Rect::new(
            content.x,
            content.y + workspace_height,
            content.width,
            detail_height,
        ),
    )
}

pub(crate) fn sidebar_section_divider_rect(area: Rect, split_ratio: f32) -> Rect {
    let content = Rect::new(area.x, area.y, area.width.saturating_sub(1), area.height);
    if content.width == 0 || content.height < 6 {
        return Rect::default();
    }

    let (workspace_height, _) = sidebar_section_heights(content.height, split_ratio);
    Rect::new(content.x, content.y + workspace_height, content.width, 1)
}

pub(crate) fn agent_panel_entries_from(
    app: &AppState,
    _terminal_runtimes: &TerminalRuntimeRegistry,
) -> Vec<AgentPanelEntry> {
    let mut entries = app
        .workspaces
        .iter()
        .enumerate()
        .flat_map(|(ws_idx, workspace)| {
            workspace
                .pane_details(&app.terminals)
                .into_iter()
                .map(move |detail| AgentPanelEntry {
                    ws_idx,
                    tab_idx: detail.tab_idx,
                    pane_id: detail.pane_id,
                    agent_kind_label: detail.agent_kind_label,
                    state: detail.state,
                    seen: detail.seen,
                    last_agent_state_change_seq: detail.last_agent_state_change_seq,
                    tokens: detail.tokens,
                })
        })
        .collect();
    crate::app::agent_view::apply_agent_view(app, &mut entries);
    entries
}

pub(crate) fn resolved_token_spans(
    resolved: &[ResolvedToken],
    state_icon: (&str, Style),
    state_text_style: Style,
    workspace_style: Style,
    secondary_style: Style,
    custom_style: Style,
    palette: &Palette,
    max_width: usize,
) -> Vec<Span<'static>> {
    let fixed_widths = resolved
        .iter()
        .map(|token| match &token.kind {
            ResolvedTokenKind::StateIcon => display_width(state_icon.0),
            ResolvedTokenKind::GitStatus {
                ahead,
                behind,
                dirty,
            } => {
                let shown = git_status_parts(*ahead, *behind, *dirty)
                    .into_iter()
                    .filter(|(visible, _)| *visible)
                    .map(|(_, text)| display_width(&text))
                    .collect::<Vec<_>>();
                shown.iter().sum::<usize>() + shown.len().saturating_sub(1)
            }
            _ => 0,
        })
        .collect::<Vec<_>>();
    let flexible_widths = resolved
        .iter()
        .map(|token| match &token.kind {
            ResolvedTokenKind::StateText(text)
            | ResolvedTokenKind::Workspace(text)
            | ResolvedTokenKind::Tab(text)
            | ResolvedTokenKind::Pane(text)
            | ResolvedTokenKind::Agent(text)
            | ResolvedTokenKind::TerminalTitle(text)
            | ResolvedTokenKind::Branch(text)
            | ResolvedTokenKind::Custom(text) => display_width(text),
            _ => 0,
        })
        .collect::<Vec<_>>();
    let minimum_width = |active: &[bool]| {
        let indices = active
            .iter()
            .enumerate()
            .filter_map(|(index, active)| active.then_some(index))
            .collect::<Vec<_>>();
        let content = indices
            .iter()
            .map(|index| fixed_widths[*index] + usize::from(flexible_widths[*index] > 0))
            .sum::<usize>();
        let separators = indices
            .windows(2)
            .map(|pair| display_width(tokens::separator(&resolved[pair[0]], &resolved[pair[1]])))
            .sum::<usize>();
        content + separators
    };
    let mut active = resolved.iter().map(|_| true).collect::<Vec<_>>();
    if minimum_width(&active) > max_width {
        for (index, width) in flexible_widths.iter().enumerate() {
            if *width > 0 {
                active[index] = false;
            }
        }
        for index in (0..resolved.len()).rev() {
            if flexible_widths[index] == 0 {
                continue;
            }
            active[index] = true;
            if minimum_width(&active) > max_width {
                active[index] = false;
            }
        }
    }
    let visible_indices = active
        .iter()
        .enumerate()
        .filter_map(|(index, active)| active.then_some(index))
        .collect::<Vec<_>>();
    let separator_width = visible_indices
        .windows(2)
        .map(|pair| display_width(tokens::separator(&resolved[pair[0]], &resolved[pair[1]])))
        .sum::<usize>();
    let fixed_width = visible_indices
        .iter()
        .map(|index| fixed_widths[*index])
        .sum::<usize>();
    let mut budgets = flexible_widths
        .iter()
        .enumerate()
        .map(|(index, width)| usize::from(active[index] && *width > 0))
        .collect::<Vec<_>>();
    let minimum = budgets.iter().sum::<usize>();
    let mut remaining = max_width
        .saturating_sub(separator_width + fixed_width)
        .saturating_sub(minimum);
    while remaining > 0 {
        let mut grew = false;
        for (budget, width) in budgets.iter_mut().zip(&flexible_widths) {
            if *budget > 0 && *budget < *width {
                *budget += 1;
                remaining -= 1;
                grew = true;
                if remaining == 0 {
                    break;
                }
            }
        }
        if !grew {
            break;
        }
    }

    let mut spans = Vec::new();
    for (position, index) in visible_indices.iter().copied().enumerate() {
        let token = &resolved[index];
        if position > 0 {
            let previous = &resolved[visible_indices[position - 1]];
            spans.push(Span::styled(
                tokens::separator(previous, token),
                Style::default()
                    .fg(palette.overlay0)
                    .add_modifier(Modifier::DIM),
            ));
        }
        match &token.kind {
            ResolvedTokenKind::StateIcon => spans.push(Span::styled(
                state_icon.0.to_string(),
                apply_token_style(state_icon.1, token.style),
            )),
            ResolvedTokenKind::StateText(text) => spans.push(Span::styled(
                truncate_end(text, budgets[index]),
                apply_token_style(state_text_style, token.style),
            )),
            ResolvedTokenKind::Workspace(text) => spans.push(Span::styled(
                truncate_end(text, budgets[index]),
                apply_token_style(workspace_style, token.style),
            )),
            ResolvedTokenKind::Tab(text)
            | ResolvedTokenKind::Pane(text)
            | ResolvedTokenKind::Agent(text)
            | ResolvedTokenKind::Branch(text) => spans.push(Span::styled(
                truncate_end(text, budgets[index]),
                apply_token_style(secondary_style, token.style),
            )),
            ResolvedTokenKind::GitStatus {
                ahead,
                behind,
                dirty,
            } => {
                let colors = [palette.green, palette.red, palette.yellow];
                let mut wrote = false;
                for ((visible, text), color) in git_status_parts(*ahead, *behind, *dirty)
                    .into_iter()
                    .zip(colors)
                {
                    if !visible {
                        continue;
                    }
                    if wrote {
                        spans.push(Span::styled(
                            " ",
                            apply_token_style(Style::default(), token.style),
                        ));
                    }
                    spans.push(Span::styled(
                        text,
                        apply_token_style(Style::default().fg(color), token.style),
                    ));
                    wrote = true;
                }
            }
            ResolvedTokenKind::TerminalTitle(text) | ResolvedTokenKind::Custom(text) => {
                spans.push(Span::styled(
                    truncate_end(text, budgets[index]),
                    apply_token_style(custom_style, token.style),
                ));
            }
        }
    }
    spans
}

fn apply_token_style(mut style: Style, patch: crate::config::SidebarTokenStyle) -> Style {
    if let Some(foreground) = patch.fg {
        style = style.fg(foreground.ratatui());
    }
    if let Some(bold) = patch.bold {
        style = if bold {
            style.add_modifier(Modifier::BOLD)
        } else {
            style.remove_modifier(Modifier::BOLD)
        };
    }
    if let Some(dim) = patch.dim {
        style = if dim {
            style.add_modifier(Modifier::DIM)
        } else {
            style.remove_modifier(Modifier::DIM)
        };
    }
    style
}

/// The git status parts in draw order, each with whether it shows. The width
/// pass and the span pass both read this so they cannot disagree.
fn git_status_parts(ahead: usize, behind: usize, dirty: usize) -> [(bool, String); 3] {
    [
        (ahead > 0, format!("↑{ahead}")),
        (behind > 0, format!("↓{behind}")),
        (dirty > 0, format!("~{dirty}")),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state::Palette;

    fn git_status_spans(
        kind: ResolvedTokenKind,
        style: crate::config::SidebarTokenStyle,
    ) -> Vec<Span<'static>> {
        let palette = Palette::catppuccin();
        resolved_token_spans(
            &[ResolvedToken { kind, style }],
            ("●", Style::default()),
            Style::default(),
            Style::default(),
            Style::default(),
            Style::default(),
            &palette,
            80,
        )
    }

    #[test]
    fn git_status_spans_color_each_part() {
        let palette = Palette::catppuccin();
        let spans = git_status_spans(
            ResolvedTokenKind::GitStatus {
                ahead: 1,
                behind: 2,
                dirty: 3,
            },
            crate::config::SidebarTokenStyle::default(),
        );

        assert_eq!(spans.len(), 5);
        assert_eq!(spans[0].content.as_ref(), "↑1");
        assert_eq!(spans[0].style.fg, Some(palette.green));
        assert_eq!(spans[1].content.as_ref(), " ");
        assert_eq!(spans[2].content.as_ref(), "↓2");
        assert_eq!(spans[2].style.fg, Some(palette.red));
        assert_eq!(spans[3].content.as_ref(), " ");
        assert_eq!(spans[4].content.as_ref(), "~3");
        assert_eq!(spans[4].style.fg, Some(palette.yellow));
    }

    #[test]
    fn git_status_spans_hide_zero_parts() {
        let spans = git_status_spans(
            ResolvedTokenKind::GitStatus {
                ahead: 0,
                behind: 2,
                dirty: 0,
            },
            crate::config::SidebarTokenStyle::default(),
        );

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "↓2");
    }

    #[test]
    fn git_status_style_override_applies_to_every_part() {
        // SidebarTokenColor has no public constructor outside the config
        // module, so build the override through the same TOML parsing
        // path a user's config file would take.
        let config: crate::config::Config = toml::from_str(
            r##"
[ui.sidebar.agents]
rows = [[{ token = "git_status", fg = "#ff00aa" }]]
"##,
        )
        .expect("valid sidebar config");
        let (_, style) = config.ui.sidebar.agents.rows[0][0].parts();
        let expected_fg = style.fg.expect("fg override present").ratatui();

        let spans = git_status_spans(
            ResolvedTokenKind::GitStatus {
                ahead: 1,
                behind: 2,
                dirty: 3,
            },
            style,
        );

        assert_eq!(spans[0].style.fg, Some(expected_fg));
        assert_eq!(spans[2].style.fg, Some(expected_fg));
        assert_eq!(spans[4].style.fg, Some(expected_fg));
    }
}
