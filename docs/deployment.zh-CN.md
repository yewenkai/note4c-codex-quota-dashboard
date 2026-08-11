# Mac 与公网服务器部署

本文使用 `quota.example.com`、`NOTE4C_LAN_IP`、`YOUR_NAME` 等占位符。不要把真实服务器地址、邮箱或密钥提交到仓库。

## 网络要求

| 方向 | 端口 | 用途 |
| --- | --- | --- |
| Mac → OpenAI | HTTPS 443 | `codex-auth list --api` 刷新额度 |
| Mac → 公网服务器 | SSH 22 | 上传帧、预览和 manifest |
| NOTE4C → 公网服务器 | HTTPS 443 | 只读拉取 manifest 和帧 |
| 浏览器 → 公网服务器 | HTTPS 443 | 查看预览 |
| 公网 → 服务器 | HTTP 80 | ACME HTTP-01 签发/续期证书，可选 |

服务器本身不需要访问 OpenAI。若 Mac 访问 OpenAI 需要代理，应按 `codex-auth` 的说明在 Mac 配置；不要把代理密钥写进本项目模板。

## 服务器：独立发布用户

先在 Mac 生成仅供本项目使用的 SSH 密钥：

```bash
ssh-keygen -t ed25519 \
  -f "$HOME/.ssh/note4c_quota_publisher_ed25519" \
  -C note4c-publisher
```

把公钥和 `deploy/server/prepare-publisher.sh` 临时复制到服务器，再以 root 执行：

```bash
bash prepare-publisher.sh note4c_quota_publisher_ed25519.pub
```

脚本创建无特权用户 `note4c-publisher` 和 `/srv/note4c/public`。它不会修改 root 的 SSH 设置。首次连接时，从 Mac 人工核对并接受服务器 host key：

```bash
ssh -i "$HOME/.ssh/note4c_quota_publisher_ed25519" \
  note4c-publisher@quota.example.com true
```

## 服务器：Nginx、TLS 和只读密码

以下以 Debian/Ubuntu 和域名为例。先安装软件并用临时 HTTP 配置完成首次签发：

```bash
sudo apt update
sudo apt install nginx apache2-utils certbot
sudo install -d -m 0755 /var/www/certbot
sudo install -m 0644 deploy/server/nginx-acme-bootstrap.conf.example \
  /etc/nginx/sites-available/note4c-acme-bootstrap
```

把临时配置中的 `quota.example.com` 替换为自己的域名，然后启用并签发：

```bash
sudo rm -f /etc/nginx/sites-enabled/default
sudo ln -s /etc/nginx/sites-available/note4c-acme-bootstrap \
  /etc/nginx/sites-enabled/note4c-acme-bootstrap
sudo nginx -t
sudo systemctl enable --now nginx
sudo certbot certonly --webroot -w /var/www/certbot -d quota.example.com
```

确保域名的 A/AAAA 记录指向服务器，安全组已开放 80 和 443。然后创建随机的设备只读密码：

```bash
sudo htpasswd -c /etc/nginx/note4c.htpasswd note4c
```

这个密码只用于读取静态看板，不能 SSH 登录或上传。请使用密码管理器生成高强度随机值。

把首页和 Nginx 模板复制到服务器：

```bash
sudo install -m 0644 deploy/server/index.html /srv/note4c/public/index.html
sudo install -m 0644 deploy/server/nginx-note4c.conf.example \
  /etc/nginx/sites-available/note4c-codex-quota
```

在正式配置中把所有 `quota.example.com` 替换为自己的域名，再切换站点并安装续期后的 reload hook：

```bash
sudo ln -s /etc/nginx/sites-available/note4c-codex-quota \
  /etc/nginx/sites-enabled/note4c-codex-quota
sudo rm /etc/nginx/sites-enabled/note4c-acme-bootstrap
sudo install -m 0755 deploy/server/certbot-deploy-hook.sh \
  /etc/letsencrypt/renewal-hooks/deploy/reload-nginx.sh
sudo nginx -t
sudo systemctl reload nginx
```

如果已有站点占用相同 `server_name`，请先处理冲突。证书续期由 Certbot 的 systemd timer 管理；可以用 `systemctl list-timers | grep certbot` 和 `sudo certbot renew --dry-run` 检查。

### 没有域名时

可以直接使用固定公网 IP，但需同时满足：

1. TLS 证书的 SAN 包含该公网 IP；
2. 证书链能被 ESP-IDF CA bundle 信任；
3. Nginx 以该证书在公网 HTTPS 端口提供服务。

不能使用自签名证书，也不能把 `https://` 写成普通 HTTP。若服务商只允许在非标准端口提供 TLS，可使用 `https://PUBLIC_IP:PORT`，并同步开放安全组端口。仓库不包含任何真实 IP 或特定云厂商配置。

## Mac：发布配置

编辑 `deploy/macos/note4c-sync.example.json` 的副本：

- `registryPath`：`codex-auth` 注册表的绝对路径；
- `codexAuthBin`：`command -v codex-auth` 的输出；
- `expectedPaidAccounts`：当前布局必须为 `3`；
- `maximumCacheAgeSeconds`：文件监听发布允许的最大缓存年龄；
- `accountLabels`：可选的邮箱到显示别名映射；
- `stateDirectory`：本地生成文件目录；
- `publisher.host`：域名或公网 IP，不写 `https://`；
- `publisher.identityFile`：专用 SSH 私钥的绝对路径。

先手工执行一次 `sync --refresh`。只有 3 个付费账号都得到实时成功响应时才会发布；Free 账号的额度错误不会阻断。

浏览器验收：

```bash
curl -u 'note4c:YOUR_RANDOM_READ_ONLY_PASSWORD' \
  https://quota.example.com/manifest.json
```

不要把密码直接写进长期脚本或 shell history；上面的命令只用于说明。日常可直接用浏览器的 Basic Auth 提示框。

## Mac：LaunchAgent 调度

复制两个 plist 到 `~/Library/LaunchAgents/`，把其中每个 `/ABSOLUTE/PATH` 和 `YOUR_NAME` 换成真实绝对路径：

```bash
cp deploy/macos/com.note4c-codex-quota.schedule.plist.example \
  "$HOME/Library/LaunchAgents/io.github.note4c-codex-quota.schedule.plist"
cp deploy/macos/com.note4c-codex-quota.watch.plist.example \
  "$HOME/Library/LaunchAgents/io.github.note4c-codex-quota.watch.plist"
```

plist 不能使用 `~` 或 `$HOME`。加载任务：

```bash
launchctl bootstrap "gui/$(id -u)" \
  "$HOME/Library/LaunchAgents/io.github.note4c-codex-quota.schedule.plist"
launchctl bootstrap "gui/$(id -u)" \
  "$HOME/Library/LaunchAgents/io.github.note4c-codex-quota.watch.plist"
```

修改 plist 后，先 `launchctl bootout` 再重新 `bootstrap`。日志默认位于 `/tmp/note4c-codex-quota-*.log` 和 `.err`。

## 故障时的保留策略

- `codex-auth` 刷新失败：不生成、不发布；
- 付费账号不是恰好 3 个：不发布；
- 文件监听遇到超过 5 分钟的缓存：不发布；
- SSH/SCP 失败：旧 manifest 保持不变；
- NOTE4C 遇到 DNS、TLS、Basic Auth、大小或 SHA-256 错误：保留上一张已验证画面；
- manifest revision 未变化：不写存储、不刷新墨水屏。
