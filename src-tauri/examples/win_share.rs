// 验证：两个 webview 窗口共用同一个 data_directory 能不能都创建成功。
// （XHSdown 的登录窗与隐藏取数窗靠共目录共享登录态，这条不成立就得换设计。）
// 运行：cargo run --example win_share

fn main() {
    let dir = std::env::temp_dir().join("xhsdown-sharedir-test");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建测试目录");

    tauri::Builder::default()
        .setup(move |app| {
            let dir2 = dir.clone();
            let w1 = tauri::WebviewWindowBuilder::new(
                app,
                "w1",
                tauri::WebviewUrl::External("about:blank".parse().unwrap()),
            )
            .visible(false)
            .data_directory(dir.clone())
            .build();
            println!(
                "窗口1（先建，取数窗角色）: {}",
                match &w1 {
                    Ok(_) => "创建成功".to_string(),
                    Err(e) => format!("失败：{e}"),
                }
            );

            let w2 = tauri::WebviewWindowBuilder::new(
                app,
                "w2",
                tauri::WebviewUrl::External("about:blank".parse().unwrap()),
            )
            .visible(false)
            .data_directory(dir2)
            .build();
            println!(
                "窗口2（后建，登录窗角色，同一目录）: {}",
                match &w2 {
                    Ok(_) => "创建成功".to_string(),
                    Err(e) => format!("失败：{e}"),
                }
            );

            // 结论
            match (&w1, &w2) {
                (Ok(_), Ok(_)) => println!("结论：共目录两个窗口可行"),
                _ => println!("结论：共目录不可行，需要改成单窗口设计"),
            }
            app.handle().exit(0);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("运行失败");
}
