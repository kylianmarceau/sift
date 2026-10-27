//! Check whether macOS accepts the requested workspace keys. Run with other
//! window managers and Sift closed so they do not hold the same bindings.
use global_hotkey::{
    GlobalHotKeyManager,
    hotkey::{Code, HotKey, Modifiers},
};

fn main() -> Result<(), String> {
    let manager = GlobalHotKeyManager::new().map_err(|e| e.to_string())?;
    let mut registered = Vec::new();
    let codes = [
        Code::Digit1,
        Code::Digit2,
        Code::Digit3,
        Code::Digit4,
        Code::Digit5,
        Code::Digit6,
        Code::Digit7,
        Code::Digit8,
        Code::Digit9,
    ];
    for code in codes {
        for modifiers in [Modifiers::ALT, Modifiers::ALT | Modifiers::SHIFT] {
            let key = HotKey::new(Some(modifiers), code);
            if let Err(error) = manager.register(key) {
                for previous in registered {
                    let _ = manager.unregister(previous);
                }
                return Err(format!("Could not register {key}: {error}"));
            }
            registered.push(key);
        }
    }
    for key in registered {
        manager.unregister(key).map_err(|e| e.to_string())?;
    }
    println!("macOS accepted Option–1…9 and Option–Shift–1…9");
    Ok(())
}
