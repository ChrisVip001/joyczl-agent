// The Joy desktop shell: one window holding the cockpit front end (frontendDist is packed into
// the binary).
//
// There are no custom commands — the shell's whole value is «a Joy that is always running, has an
// icon, and is one click away».
// Which server to talk to is the front end's business (the sign-in card's server field); the
// shell does not overstep.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("the Joy desktop shell cannot start");
}
