#!/usr/bin/env bash
set -euo pipefail

if [[ $EUID -ne 0 ]]; then
  echo "请以 root 运行" >&2
  exit 1
fi
if [[ $# -ne 1 ]]; then
  echo "用法：$0 /path/to/note4c_publisher_ed25519.pub" >&2
  exit 1
fi

public_key_file=$1
if [[ ! -f $public_key_file ]]; then
  echo "公钥文件不存在：$public_key_file" >&2
  exit 1
fi
IFS= read -r public_key < "$public_key_file"
if [[ $public_key != ssh-ed25519\ * ]]; then
  echo "只接受独立的 Ed25519 公钥" >&2
  exit 1
fi

if ! id note4c-publisher >/dev/null 2>&1; then
  useradd --system --create-home --shell /bin/bash note4c-publisher
fi
# Ubuntu's OpenSSH rejects public-key login for a newly created account whose
# password field is locked. Give the account an unknown random password hash;
# SSH still uses the dedicated Ed25519 key and no password is disclosed.
publisher_password_hash=$(openssl passwd -6 "$(openssl rand -base64 48)")
usermod --password "$publisher_password_hash" note4c-publisher
unset publisher_password_hash

install -d -m 0755 -o note4c-publisher -g note4c-publisher /srv/note4c/public
install -d -m 0755 -o note4c-publisher -g note4c-publisher /srv/note4c/public/frames
install -d -m 0755 -o root -g root /srv/note4c/acme
install -d -m 0700 -o note4c-publisher -g note4c-publisher /home/note4c-publisher/.ssh
install -m 0600 -o note4c-publisher -g note4c-publisher /dev/null \
  /home/note4c-publisher/.ssh/authorized_keys
printf 'restrict %s\n' "$public_key" > /home/note4c-publisher/.ssh/authorized_keys
chown note4c-publisher:note4c-publisher /home/note4c-publisher/.ssh/authorized_keys

echo "已准备独立发布用户和 /srv/note4c/public；未修改 root 登录配置。"
