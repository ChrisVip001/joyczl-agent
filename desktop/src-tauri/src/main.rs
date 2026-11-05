// Joy 桌面壳：一个窗口，装着驾驶舱前端（frontendDist 打包进二进制）。
//
// 没有任何自定义命令——桌面壳的全部价值是「有一个常驻的、有图标的、点开就到的 Joy」。
// 连哪台服务器是前端的事（登录卡的服务器地址字段），壳不越界。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("Joy 桌面壳起不来");
}
