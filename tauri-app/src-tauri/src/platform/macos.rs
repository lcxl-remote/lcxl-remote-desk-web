use tauri::Manager;

pub fn block_input(block: bool) -> Result<(), String> {
    // macOS: 使用 CGEventTap 拦截输入
    // 需要辅助功能权限 (Accessibility)
    // 具体实现：
    // 1. 创建 CGEventTap（kCGSessionEventTap, kCGHeadInsertEventTap）
    // 2. 在回调中返回 NULL 来拦截事件
    // 3. block=false 时移除 tap
    log::warn!("macOS block_input: implementation needed via CGEventTap");
    if block {
        log::info!("macOS: Would block input (needs accessibility permission)");
    } else {
        log::info!("macOS: Would unblock input");
    }
    Ok(())
}
