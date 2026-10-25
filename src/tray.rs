use std::sync::mpsc::{self, Receiver};

use anyhow::Result;
use eframe::egui;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::icon;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Show,
    SyncNow,
    OpenFolder,
    Quit,
}

pub struct Tray {
    icon: TrayIcon,
    actions: Receiver<Action>,
    tooltip: String,
}

impl Tray {
    /// Must be called on the UI thread. Tray events wake egui so that
    /// `App::logic` can handle them even while the window is hidden.
    pub fn new(ctx: &egui::Context) -> Result<Self> {
        let menu = Menu::new();
        let items = [
            (
                MenuItem::with_id("show", "Open iPhone Photo Sync", true, None),
                Action::Show,
            ),
            (
                MenuItem::with_id("sync", "Sync now", true, None),
                Action::SyncNow,
            ),
            (
                MenuItem::with_id("folder", "Open photos folder", true, None),
                Action::OpenFolder,
            ),
        ];
        for (item, _) in &items {
            menu.append(item)?;
        }
        menu.append(&PredefinedMenuItem::separator())?;
        let quit = MenuItem::with_id("quit", "Quit", true, None);
        menu.append(&quit)?;

        let ids: Vec<_> = items
            .iter()
            .map(|(item, action)| (item.id().clone(), *action))
            .chain([(quit.id().clone(), Action::Quit)])
            .collect();

        let (tx, actions) = mpsc::channel();
        {
            let (tx, ctx) = (tx.clone(), ctx.clone());
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                if let Some((_, action)) = ids.iter().find(|(id, _)| *id == e.id) {
                    let _ = tx.send(*action);
                    ctx.request_repaint();
                }
            }));
        }
        {
            let ctx = ctx.clone();
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                let show = matches!(
                    e,
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } | TrayIconEvent::DoubleClick { .. }
                );
                if show {
                    let _ = tx.send(Action::Show);
                    ctx.request_repaint();
                }
            }));
        }

        let tooltip = "iPhone Photo Sync".to_owned();
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip(&tooltip)
            .with_icon(Icon::from_rgba(icon::rgba(), icon::SIZE, icon::SIZE)?)
            .build()?;
        Ok(Self {
            icon,
            actions,
            tooltip,
        })
    }

    pub fn actions(&self) -> impl Iterator<Item = Action> + '_ {
        self.actions.try_iter()
    }

    pub fn set_tooltip(&mut self, text: &str) {
        if self.tooltip != text {
            self.tooltip = text.to_owned();
            let _ = self.icon.set_tooltip(Some(text));
        }
    }
}
