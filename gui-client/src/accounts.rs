//! Microsoft device-code login, the stored-account list, and skin-face
//! avatars. All networking runs on the shared tokio runtime; results come
//! back over channels exactly like the backup/pack jobs do.

use std::sync::mpsc::{Receiver, Sender};

use eframe::egui;
use launchcraft_auth::{
    AccountManager, DeviceCodeResponse, LaunchcraftAccount, MicrosoftAuthClient,
};
use uuid::Uuid;

use crate::java;

/// Progress messages of a running device-code login.
pub enum LoginEvent {
    /// The flow started; show the code and open the verification URL.
    Code {
        flow: DeviceCodeResponse,
        /// True when the browser could not be opened automatically.
        open_failed: bool,
    },
    /// One poll completed while the user has not consented yet.
    Pending,
    /// Login finished (Ok(account) = success).
    Done(Result<LaunchcraftAccount, String>),
}

/// Everything the Accounts tab renders and mutates.
pub struct AccountState {
    pub manager: AccountManager,
    /// Live login flow (device code shown, poll running).
    pub flow: Option<DeviceCodeResponse>,
    pub login_open_failed: bool,
    pub login_pending: bool,
    pub login_error: Option<String>,
    login_tx: Sender<LoginEvent>,
    login_rx: Receiver<LoginEvent>,
    /// Snapshot of the store, refreshed after every change.
    pub accounts: Vec<LaunchcraftAccount>,
    pub active: Option<Uuid>,
    /// 64×64 face texture of the active account + who it belongs to.
    pub avatar: Option<egui::TextureHandle>,
    avatar_of: Option<Uuid>,
    avatar_tx: Sender<Option<egui::ColorImage>>,
    avatar_rx: Receiver<Option<egui::ColorImage>>,
}

impl AccountState {
    pub fn new(config_dir: &std::path::Path, ctx: &egui::Context) -> Self {
        let (login_tx, login_rx) = std::sync::mpsc::channel();
        let (avatar_tx, avatar_rx) = std::sync::mpsc::channel();
        let manager = AccountManager::new(config_dir.join("accounts.json"))
            .expect("could not open accounts.json");
        let mut state = Self {
            manager,
            flow: None,
            login_open_failed: false,
            login_pending: false,
            login_error: None,
            login_tx,
            login_rx,
            accounts: Vec::new(),
            active: None,
            avatar: None,
            avatar_of: None,
            avatar_tx,
            avatar_rx,
        };
        state.refresh();
        let _ = ctx;
        state
    }

    /// Re-read the store snapshot.
    pub fn refresh(&mut self) {
        let mut accounts = self.manager.list_accounts().unwrap_or_default();
        accounts.sort_by_key(|a| a.username.to_lowercase());
        self.active = self
            .manager
            .get_active_account()
            .ok()
            .flatten()
            .map(|a| a.uuid);
        self.accounts = accounts;
    }

    pub fn active_account(&self) -> Option<&LaunchcraftAccount> {
        let uuid = self.active?;
        self.accounts.iter().find(|a| a.uuid == uuid)
    }

    /// Move login + avatar events into the UI state; returns login results
    /// (Ok account already saved to the store by the caller-side drain).
    pub fn drain(&mut self) -> Vec<Result<LaunchcraftAccount, String>> {
        let mut finished = Vec::new();
        while let Ok(event) = self.login_rx.try_recv() {
            match event {
                LoginEvent::Code { flow, open_failed } => {
                    self.login_open_failed = open_failed;
                    self.login_pending = true;
                    self.login_error = None;
                    self.flow = Some(flow);
                }
                LoginEvent::Pending => self.login_pending = true,
                LoginEvent::Done(result) => {
                    self.flow = None;
                    self.login_pending = false;
                    finished.push(result);
                }
            }
        }
        finished
    }

    /// Pick up a finished avatar image (decoded off-thread).
    pub fn drain_avatar(&mut self, ctx: &egui::Context) {
        while let Ok(image) = self.avatar_rx.try_recv() {
            self.avatar =
                image.map(|img| ctx.load_texture("avatar", img, egui::TextureOptions::NEAREST));
        }
    }

    pub fn set_active(&mut self, uuid: Uuid) {
        if self.manager.set_active_account(uuid).is_ok() {
            self.refresh();
        }
    }

    pub fn remove(&mut self, uuid: Uuid) {
        if self.manager.remove_account(&uuid).is_ok() {
            if self.avatar_of == Some(uuid) {
                self.avatar = None;
                self.avatar_of = None;
            }
            self.refresh();
        }
    }

    pub fn store_account(&mut self, account: LaunchcraftAccount) {
        let _ = self.manager.save_account(account);
        self.refresh();
    }

    /// Fetch (or clear) the face texture for the active account.
    pub fn request_avatar(&mut self, rt: &tokio::runtime::Runtime, ctx: &egui::Context) {
        let account = match self.active_account() {
            Some(a) => a.clone(),
            None => {
                self.avatar = None;
                self.avatar_of = None;
                return;
            }
        };
        if self.avatar_of == Some(account.uuid) && self.avatar.is_some() {
            return;
        }
        self.avatar_of = Some(account.uuid);
        let skin_url = match account.skin_url.clone() {
            Some(url) => url,
            None => {
                self.avatar = None;
                let _ = self.avatar_tx.send(None);
                return;
            }
        };
        let tx = self.avatar_tx.clone();
        let ctx = ctx.clone();
        rt.spawn(async move {
            let image = fetch_face(&skin_url).await;
            let _ = tx.send(image);
            ctx.request_repaint();
        });
    }

    pub fn tx_login(&self) -> Sender<LoginEvent> {
        self.login_tx.clone()
    }
}

/// Start a device-code login on the runtime: the code comes back in a
/// `LoginEvent::Code`, then polling runs until the user consents.
pub fn spawn_login(rt: &tokio::runtime::Runtime, ctx: egui::Context, tx: Sender<LoginEvent>) {
    rt.spawn(async move {
        let auth = MicrosoftAuthClient::new(std::env::var("LAUNCHCRAFT_CLIENT_ID").ok());
        let flow = match auth.initiate_device_flow().await {
            Ok(flow) => flow,
            Err(e) => {
                let _ = tx.send(LoginEvent::Done(Err(e.to_string())));
                ctx.request_repaint();
                return;
            }
        };
        let open_failed = open_url(&flow.verification_uri).is_err();
        let _ = tx.send(LoginEvent::Code {
            flow: flow.clone(),
            open_failed,
        });
        ctx.request_repaint();

        let tx_poll = tx.clone();
        let result = auth
            .finish_device_login(&flow, move |_msg| {
                let _ = tx_poll.send(LoginEvent::Pending);
            })
            .await;
        let _ = tx.send(LoginEvent::Done(result.map_err(|e| e.to_string())));
        ctx.request_repaint();
    });
}

/// Re-run the full auth chain for one account (Home/PLAY calls this when
/// the Minecraft token is about to lapse; also fired by the Refresh button).
pub fn spawn_refresh(
    rt: &tokio::runtime::Runtime,
    ctx: egui::Context,
    tx: Sender<LoginEvent>,
    account: LaunchcraftAccount,
) {
    rt.spawn(async move {
        let auth = MicrosoftAuthClient::new(std::env::var("LAUNCHCRAFT_CLIENT_ID").ok());
        let result = auth
            .refresh_account(&account)
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(LoginEvent::Done(result));
        ctx.request_repaint();
    });
}

/// Download a skin PNG and cut out the 8×8 face, upscaled for display.
pub async fn fetch_face(skin_url: &str) -> Option<egui::ColorImage> {
    let bytes = reqwest::get(skin_url).await.ok()?.bytes().await.ok()?;
    face_from_skin(&bytes)
}

pub fn face_from_skin(bytes: &[u8]) -> Option<egui::ColorImage> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    // Both the classic 64×32 and modern 64×64 skin layouts put the face
    // front at (8, 8) in the head row.
    if img.width() < 64 || img.height() < 16 {
        return None;
    }
    let face = image::imageops::crop_imm(&img, 8, 8, 8, 8).to_image();
    let big = image::imageops::resize(&face, 64, 64, image::imageops::FilterType::Nearest);
    Some(egui::ColorImage::from_rgba_unmultiplied(
        [64, 64],
        big.as_raw(),
    ))
}

fn open_url(url: &str) -> std::io::Result<()> {
    let opener = java::open_program();
    std::process::Command::new(opener)
        .arg(url)
        .spawn()
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::ImageEncoder as _;

    fn png(img: &image::RgbaImage) -> Vec<u8> {
        let mut buf = Vec::new();
        image::codecs::png::PngEncoder::new(&mut buf)
            .write_image(
                img.as_raw(),
                img.width(),
                img.height(),
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        buf
    }

    /// A 64×64 skin whose face region is opaque red and everything else blue.
    fn sample_skin() -> Vec<u8> {
        let mut img = image::RgbaImage::from_pixel(64, 64, image::Rgba([0, 0, 255, 255]));
        for y in 8..16u32 {
            for x in 8..16u32 {
                img.put_pixel(x, y, image::Rgba([255, 0, 0, 255]));
            }
        }
        png(&img)
    }

    #[test]
    fn face_crop_picks_the_face_region() {
        let face = face_from_skin(&sample_skin()).expect("decode sample skin");
        assert_eq!(face.size, [64, 64]);
        // Every face pixel came from the red 8×8 block.
        for p in face.pixels.iter().take(64 * 64).step_by(64) {
            assert_eq!(*p, egui::Color32::from_rgba_unmultiplied(255, 0, 0, 255));
        }
    }

    #[test]
    fn face_crop_rejects_non_skins() {
        assert!(face_from_skin(b"not a png").is_none());
        let tiny = png(&image::RgbaImage::from_pixel(
            8,
            8,
            image::Rgba([0, 0, 0, 255]),
        ));
        assert!(face_from_skin(&tiny).is_none());
    }

    #[test]
    fn account_store_roundtrip_through_state() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = egui::Context::default();
        let mut state = AccountState::new(tmp.path(), &ctx);
        assert!(state.accounts.is_empty() && state.active.is_none());

        let account = LaunchcraftAccount {
            uuid: uuid::Uuid::new_v4(),
            username: "Steve".into(),
            refresh_token: "r".into(),
            access_token: "a".into(),
            expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
            skin_url: None,
            xuid: "42".into(),
        };
        state.store_account(account.clone());
        assert_eq!(state.accounts.len(), 1);
        assert_eq!(state.active, Some(account.uuid)); // first account goes active
        assert_eq!(state.active_account().unwrap().username, "Steve");

        // Persisted to disk: a fresh state sees the same account.
        let reopened = AccountState::new(tmp.path(), &ctx);
        assert_eq!(reopened.accounts.len(), 1);
        assert_eq!(reopened.accounts[0].xuid, "42");

        state.remove(account.uuid);
        assert!(state.accounts.is_empty());
    }
}
