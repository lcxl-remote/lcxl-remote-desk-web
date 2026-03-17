# 2026-03-17 OpenH264 码率计算优化

## 1. 任务背景 (Implementation Plan)
用户反馈 OpenH264 编码器因不支持视频质量设置而导致码率计算逻辑错误（画质越差码率越高，且未考虑分辨率），造成画面模糊或码率过高导致编码器报错。本计划旨在通过引入 BPP (Bits Per Pixel) 方案，根据分辨率、帧率和 `video_quality` 动态计算目标码率，并设置安全上限。

## 2. 任务清单 (Task List)
- [x] 调研 H264 码率与分辨率关系及 OpenH264 限制
- [x] 编写实现方案并请求审核
- [x] 修改 `signal-facade/src/model/desk_settings.rs` 中的计算逻辑
- [x] 验证计算结果的合理性

## 3. 执行过程总结 (Walkthrough)

### 关键改进
1. **动态 BPP 算法**：在 `desk_settings.rs` 中实现了基于像素总数的码率计算公式。
   - **公式**: `bps = width * height * fps * bpp`
   - **BPP 映射**: 将 `video_quality` (0-63) 映射到 BPP (0.20 - 0.02)。
2. **安全保护**：为防止 OpenH264 报错，设置码率硬上限为 **100 Mbps**。
3. **前端适配**：用户已手动更新 `video_encoder_factory.rs`，使 `get_h264_encoder_settings` 等方法在实际创建编码器时生效。

### 验证结论
- **单元测试**: 在 `signal-facade` 中通过 `mod tests` 验证了 1080p、4K 场景下的码率计算准确性及上限触发逻辑。
- **实际测试**: 用户确认配置已实际生效，系统运行稳定。

---
> [!NOTE]
> 该项优化提升了远程桌面在 OpenH264 模式下的清晰控制能力，并增强了针对高分辨率屏幕的系统健壮性。
