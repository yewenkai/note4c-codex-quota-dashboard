# NOTE4C 4C 固件补丁与刷机

## 支持范围

本补丁仅针对：

- ZECTRIX NOTE4C 4C；
- ESP32-S3 N16R8；
- 400×300 SSD2683 BWRY 四色屏；
- 上游 [`LazyYoun/youn-ink-fourcolor-firmware`](https://github.com/LazyYoun/youn-ink-fourcolor-firmware) 提交 `23f5e341cb1f7ccf26bb96f2bad13d192beef5df`；
- ESP-IDF 6.0.2。

不要把它刷到 NOTE4 黑白版或其他板型。刷机有使设备暂时无法启动的风险；开始前请确认有可用的上游恢复固件，并记录原有 Wi-Fi/设备设置。

## 补丁做了什么

- 增加后台 HTTPS quota client；
- 使用 Basic Auth 读取 manifest 和 BWRY 原始帧；
- 校验 schema、尺寸、格式、路径和 SHA-256；
- 把成功画面保存为保留照片 `codexquota`；
- 新 revision 到达时自动全屏显示；
- 同一相册页面内更新时强制重绘，避免仍显示旧 framebuffer；
- 在局域网 `/settings` 接口增加看板配置，读取配置时不返回密码；
- 排除 NOTE4C 没有使用的 camera/video 组件，使固定版本能在 ESP-IDF 6.0.2 下构建。

固件从不保存 Codex/OpenAI 令牌，也不访问 OpenAI。

## 应用补丁

```bash
git clone https://github.com/LazyYoun/youn-ink-fourcolor-firmware.git
cd youn-ink-fourcolor-firmware
git checkout 23f5e341cb1f7ccf26bb96f2bad13d192beef5df
git apply --check \
  /path/to/note4c-codex-quota-dashboard/firmware/patches/0001-note4c-4c-codex-quota-dashboard.patch
git apply \
  /path/to/note4c-codex-quota-dashboard/firmware/patches/0001-note4c-4c-codex-quota-dashboard.patch
```

如果 `git apply --check` 失败，不要强行应用到更新的上游版本；先对比上游变更并移植补丁。

## 构建

安装并进入 ESP-IDF 6.0.2 环境后：

```bash
cd firmware
idf.py set-target esp32s3
idf.py menuconfig
idf.py build
```

在 `menuconfig` 中确认：

- Board type 为 `zectrix-s3-epaper-4.2`；
- E-paper panel 为 `4-color BWRY SSD2683`；
- Flash 为 16 MB；
- PSRAM 已启用且与 N16R8 匹配。

上游的 `sdkconfig.defaults.esp32s3` 已默认选择 4C 面板，但刷机前仍建议人工复核。

## 烧录

先找到串口：

```bash
ls /dev/cu.usbmodem*
```

再构建并烧录，示例端口仅为占位：

```bash
idf.py -p /dev/cu.usbmodemXXXX flash
idf.py -p /dev/cu.usbmodemXXXX monitor
```

按 `Ctrl-]` 退出 monitor。不要在文档、Issue 或日志中上传包含 Wi-Fi 密码、服务密码或真实服务器地址的完整启动日志。

## 配置额度服务

设备连接 Wi-Fi 后，在可信局域网执行：

```bash
curl -X POST "http://NOTE4C_LAN_IP/settings" \
  -H 'Content-Type: application/json' \
  --data '{
    "codex_dashboard": {
      "enabled": true,
      "base_url": "https://quota.example.com",
      "username": "note4c",
      "password": "YOUR_RANDOM_READ_ONLY_PASSWORD",
      "poll_minutes": 5
    }
  }'
```

查询状态：

```bash
curl "http://NOTE4C_LAN_IP/settings"
```

响应只显示 `enabled`、`configured`、`base_url`、`username` 和 `poll_minutes`，不会返回密码。POST 成功后会立即请求一次同步；以后每 1–60 分钟轮询，推荐值为 5。

## 串口验收关键字

成功链路通常会看到：

```text
CodexQuota: Stored verified frame revision=...
```

revision 未变化时：

```text
CodexQuota: Manifest unchanged: ...
```

任何下载或校验失败都会保留上一张成功画面。画面中的黄色“当前”来自 Mac 端 `codex-auth` 的 `active_account_key`。
