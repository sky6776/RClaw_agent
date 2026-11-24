#!/usr/bin/env bash
# RClaw 多端同步 · 阿里云 ECS 一键 WebDAV 文件服务
#
# 用法（在 ECS 上以 root 运行）：
#   sudo bash deploy-rclaw-webdav-aliyun.sh
#   sudo RCLAW_PASS='你自己定的密码' bash deploy-rclaw-webdav-aliyun.sh
#   sudo RCLAW_DOMAIN=sync.example.com RCLAW_CERT=/path/fullchain.pem \
#        RCLAW_KEY=/path/privkey.pem bash deploy-rclaw-webdav-aliyun.sh   # 有域名/证书时
#   bash deploy-rclaw-webdav-aliyun.sh --print-only                       # 只打印将写入的配置
#
# 做的事：装 Apache + mod_dav + mod_ssl，建专用目录与 Basic 账号，起 HTTPS（默认自签证书 +
# 指纹固定，不需要域名、不需要备案），最后用 curl 实测 PROPFIND 与条件创建（If-None-Match）。
# 幂等：重复运行会沿用已有账号密码与证书（不会悄悄换掉已固定的指纹），只补齐缺的部分。
#
# 环境变量（都有默认值）：
#   RCLAW_PORT        监听端口，默认 8443
#   RCLAW_USER        Basic 用户名，默认 rclaw
#   RCLAW_PASS        密码；不填则自动生成并只打印这一次（重跑时沿用已存的密码）
#   RCLAW_DIR         同步目录，默认 /srv/rclaw-sync
#   RCLAW_HOST        客户端填写的主机名/IP；不填则自动探测公网 IP
#   RCLAW_DOMAIN      有域名时填（配合 RCLAW_CERT/RCLAW_KEY；本脚本不代办 ACME）
#   RCLAW_CERT/KEY    已有证书/私钥路径（不填则自签，10 年，SAN 含公网 IP）
#   RCLAW_ALLOW_IPS   只允许这些来源（空格分隔的 IP/CIDR），不填则不限制；本机回环
#                     自动放行（脚本要用它做自检）
#   RCLAW_REGENERATE_CERT=1  强制重新自签（会让所有设备已固定的指纹失效，慎用）
set -euo pipefail

PORT=${RCLAW_PORT:-8443}
USER_NAME=${RCLAW_USER:-rclaw}
DIR=${RCLAW_DIR:-/srv/rclaw-sync}
ETC=/etc/rclaw-webdav
LOCK_DIR=/var/lib/rclaw-dav
HOST=${RCLAW_HOST:-}
DOMAIN=${RCLAW_DOMAIN:-}
CERT_SRC=${RCLAW_CERT:-}
KEY_SRC=${RCLAW_KEY:-}
ALLOW_IPS=${RCLAW_ALLOW_IPS:-}
PASSWORD=${RCLAW_PASS:-}
PRINT_ONLY=0
[ "${1:-}" = "--print-only" ] && PRINT_ONLY=1

say()  { printf '%s\n' "$*"; }
fail() { printf '错误：%s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------- 环境探测
if [ "$PRINT_ONLY" = 1 ]; then
    FAMILY=${RCLAW_FAMILY:-debian}
else
    [ "$(id -u)" = 0 ] || fail "请用 root 运行（sudo bash $0）"
    . /etc/os-release 2>/dev/null || fail "读不到 /etc/os-release，无法识别发行版"
    case " ${ID:-} ${ID_LIKE:-} " in
        *debian*|*ubuntu*) FAMILY=debian ;;
        *rhel*|*centos*|*fedora*|*alinux*) FAMILY=rhel ;;
        *) fail "不支持的发行版 ${PRETTY_NAME:-$ID}（支持 Debian/Ubuntu 与 Alibaba Cloud Linux/CentOS/RHEL）" ;;
    esac
    if [ -z "$HOST" ] && [ -z "$DOMAIN" ]; then
        HOST=$(curl -fsS --max-time 3 http://100.100.100.200/latest/meta-data/eipv4 2>/dev/null \
            || curl -fsS --max-time 3 http://100.100.100.200/latest/meta-data/public-ipv4 2>/dev/null \
            || curl -fsS --max-time 5 https://api.ipify.org 2>/dev/null \
            || true)
        [ -n "$HOST" ] || fail "探测不到公网 IP：请设置 RCLAW_HOST=<ECS 公网 IP 或域名>"
    fi
fi
CLIENT_HOST=${DOMAIN:-${HOST:-<ECS公网IP>}}

if [ "$FAMILY" = debian ]; then
    APACHE_USER=www-data; SERVICE=apache2; CONF=/etc/apache2/sites-available/rclaw-sync.conf
else
    APACHE_USER=apache;  SERVICE=httpd;  CONF=/etc/httpd/conf.d/rclaw-sync.conf
fi
CERT=$ETC/tls/cert.pem
KEY=$ETC/tls/key.pem
HTPASSWD=$ETC/htpasswd
URL="https://${CLIENT_HOST}:${PORT}/rclaw/"

# ---------------------------------------------------------------- 安装软件
install_packages() {
    if [ "$FAMILY" = debian ]; then
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -y -qq
        apt-get install -y -qq apache2 apache2-utils openssl curl
    else
        dnf install -y -q httpd mod_ssl httpd-tools openssl curl 2>/dev/null \
            || yum install -y -q httpd mod_ssl httpd-tools openssl curl
    fi
}

# ---------------------------------------------------------------- 配置渲染
render_config() {
    cat <<CONF
# 由 deploy-rclaw-webdav-aliyun.sh 生成（重复运行会整份重写）
Listen ${PORT}
<VirtualHost *:${PORT}>
    ServerName rclaw-webdav
    SSLEngine on
    SSLCertificateFile ${CERT}
    SSLCertificateKeyFile ${KEY}
    SSLProtocol all -SSLv3 -TLSv1 -TLSv1.1
    Alias /rclaw "${DIR}"
    DavLockDB ${LOCK_DIR}/DavLock
    <Directory "${DIR}">
        Dav On
        Options Indexes FollowSymLinks
        AuthType Basic
        AuthName "RClaw Sync"
        AuthUserFile ${HTPASSWD}
CONF
    # 多个 Require 在同一节里是「或」；来源限制必须和账号校验同时满足，所以包在 RequireAll。
    if [ -n "$ALLOW_IPS" ]; then
        cat <<CONF
        <RequireAll>
            Require valid-user
            Require ip 127.0.0.1 ::1 ${ALLOW_IPS}
        </RequireAll>
CONF
    else
        printf '        Require valid-user\n'
    fi
    cat <<CONF
    </Directory>
</VirtualHost>
CONF
}

if [ "$PRINT_ONLY" = 1 ]; then
    say "# 将写入 ${CONF}："
    render_config
    say "# 客户端地址：$URL"
    exit 0
fi

say "==> 安装 Apache / mod_dav / mod_ssl"
install_packages

say "==> 建目录、账号与证书（已存在的沿用）"
install -d -m 0755 "$DIR" "$LOCK_DIR" "$ETC" "$ETC/tls"
chown -R "$APACHE_USER:$APACHE_USER" "$DIR" "$LOCK_DIR"

# 自签证书：默认不重签，避免已固定的指纹失效
if [ -n "$CERT_SRC" ] || [ -n "$KEY_SRC" ]; then
    [ -f "$CERT_SRC" ] && [ -f "$KEY_SRC" ] || fail "RCLAW_CERT / RCLAW_KEY 指向的文件不存在"
    cp -f "$CERT_SRC" "$CERT"; cp -f "$KEY_SRC" "$KEY"
elif [ ! -s "$CERT" ] || [ ! -s "$KEY" ] || [ "${RCLAW_REGENERATE_CERT:-0}" = 1 ]; then
    # SAN：主机是地址就写 IP:，是域名就写 DNS:
    case "$HOST" in
        *[!0-9a-fA-F:.]*) SAN="DNS:${HOST}" ;;   # 含字母/连字符：按域名
        *)                SAN="IP:${HOST}" ;;    # 纯数字/点/冒号：按 IP
    esac
    say "    生成自签证书（SAN ${SAN}，10 年）"
    openssl req -x509 -nodes -newkey rsa:2048 -days 3650 \
        -keyout "$KEY" -out "$CERT" -subj "/CN=rclaw-webdav" \
        -addext "subjectAltName=${SAN},DNS:localhost,IP:127.0.0.1" >/dev/null 2>&1
fi
chmod 0600 "$KEY"; chmod 0644 "$CERT"

GENERATED=0
CHECK_PASS=""
if [ ! -s "$HTPASSWD" ] || ! grep -q "^${USER_NAME}:" "$HTPASSWD"; then
    [ -n "$PASSWORD" ] || { PASSWORD=$(openssl rand -base64 24 | tr -d '/+=' | cut -c1-20); GENERATED=1; }
    umask 077
    htpasswd -i -c -B "$HTPASSWD" "$USER_NAME" <<<"$PASSWORD" >/dev/null
    CHECK_PASS=$PASSWORD
elif [ -n "$PASSWORD" ]; then
    htpasswd -i -B "$HTPASSWD" "$USER_NAME" <<<"$PASSWORD" >/dev/null
    CHECK_PASS=$PASSWORD
else
    say "    Basic 账号 $USER_NAME 已存在，沿用原密码（要重置请设 RCLAW_PASS=新密码 重跑）"
fi
chown root:"$APACHE_USER" "$HTPASSWD" 2>/dev/null || chown root:root "$HTPASSWD"
chmod 0640 "$HTPASSWD"

# SELinux（Alibaba Cloud Linux / CentOS 开启时）：让 httpd 能读写同步目录
if command -v getenforce >/dev/null && [ "$(getenforce)" != Disabled ]; then
    command -v semanage >/dev/null \
        && semanage fcontext -a -t httpd_sys_rw_content_t "${DIR}(/.*)?" 2>/dev/null || true
    chcon -R -t httpd_sys_rw_content_t "$DIR" "$LOCK_DIR" 2>/dev/null || true
fi

say "==> 写入 Apache 配置并启用模块"
if [ "$FAMILY" = debian ]; then
    a2enmod dav dav_fs ssl auth_basic >/dev/null
    render_config >"$CONF"
    a2ensite rclaw-sync >/dev/null
else
    render_config >"$CONF"
    # 发行版没有启用 dav 模块时补上（必须在写进配置前判断，否则 configtest 会先报
    # "Invalid command 'Dav'）。已在 conf.modules.d 里加载过就跳过。
    if ! grep -hqs '^[[:space:]]*LoadModule dav_module' /etc/httpd/conf.modules.d/*.conf; then
        sed -i "1i LoadModule dav_module modules/mod_dav.so\nLoadModule dav_fs_module modules/mod_dav_fs.so" "$CONF"
    fi
fi

say "==> 检查配置并重启服务"
if [ "$FAMILY" = debian ]; then
    apache2ctl configtest
else
    httpd -t
fi
systemctl enable --now "$SERVICE" >/dev/null 2>&1 || systemctl restart "$SERVICE"
systemctl restart "$SERVICE"

say "==> 防火墙放行 ${PORT}/tcp"
if command -v firewall-cmd >/dev/null && firewall-cmd --state >/dev/null 2>&1; then
    firewall-cmd --permanent --add-port="${PORT}/tcp" >/dev/null && firewall-cmd --reload >/dev/null
elif command -v ufw >/dev/null && ufw status 2>/dev/null | grep -q 'Status: active'; then
    ufw allow "${PORT}/tcp" >/dev/null
else
    say "    （本机没有启用的 firewalld/ufw，跳过）"
fi

# ---------------------------------------------------------------- 自检
say "==> 自检（PROPFIND / 条件创建）"
if [ -z "$CHECK_PASS" ]; then
    say "    账号是沿用的、本脚本不知道密码：跳过自检。"
    say "    要自检请带密码重跑：RCLAW_PASS='原密码' bash $0"
    FP=$(openssl x509 -in "$CERT" -noout -fingerprint -sha256 | sed 's/^.*=//')
    say ""
    say "================================================================"
    say " RClaw WebDAV 已就绪（未自检）"
    say "   地址：    $URL"
    say "   用户名：  $USER_NAME"
    say "   证书指纹：$FP"
    say "================================================================"
    exit 0
fi
basic=(-u "${USER_NAME}:${CHECK_PASS}")
code=$(curl -sk -o /dev/null -w '%{http_code}' "${basic[@]}" -X PROPFIND \
    -H 'Depth: 0' "https://127.0.0.1:${PORT}/rclaw/") || true
[ "$code" = 207 ] || fail "PROPFIND 自检失败（HTTP ${code:-无响应}），请看 systemctl status ${SERVICE}"

# 先建 selftest/ 目录（RClaw 自己的自检也用它；已存在时 Apache 返回 405，照常继续）。
dir_code=$(curl -sk -o /dev/null -w '%{http_code}' "${basic[@]}" -X MKCOL \
    "https://127.0.0.1:${PORT}/rclaw/selftest/") || true
case "$dir_code" in
    200|201|204|301|302|405) ;;
    *) fail "MKCOL 自检失败（HTTP ${dir_code:-无响应}）：服务端不允许建目录，RClaw 的目录补建会失败" ;;
esac

probe="https://127.0.0.1:${PORT}/rclaw/selftest/cond-$(date +%s)-$$.txt"
first=$(curl -sk -o /dev/null -w '%{http_code}' "${basic[@]}" -X PUT \
    -H 'If-None-Match: *' --data-binary 'first' "$probe") || true
second=$(curl -sk -o /dev/null -w '%{http_code}' "${basic[@]}" -X PUT \
    -H 'If-None-Match: *' --data-binary 'second' "$probe") || true
kept=$(curl -fsk "${basic[@]}" "$probe") || kept=''
curl -sk "${basic[@]}" -X DELETE "$probe" >/dev/null || true
case "$first" in
    200|201|204) ;;
    *) fail "条件创建首次写入失败（HTTP ${first:-无响应}）" ;;
esac
if [ "$second" = 412 ] && [ "$kept" = first ]; then
    say "    条件创建：第二次写入被拒（412）、读回仍是第一次的内容 —— 正确"
else
    fail "条件创建验收未通过（第二次写入 HTTP ${second:-无响应}，读回内容=$kept）。这会让关闭标记可能被迟到创建覆盖，请检查 mod_dav 配置"
fi

FP=$(openssl x509 -in "$CERT" -noout -fingerprint -sha256 | sed 's/^.*=//')

say ""
say "================================================================"
say " RClaw WebDAV 已就绪"
say "   地址：    $URL"
say "   用户名：  $USER_NAME"
if [ "$GENERATED" = 1 ]; then
    say "   密码：    $PASSWORD      （只显示这一次，请立刻记下）"
else
    say "   密码：    （你设置的密码 / 或沿用旧密码）"
fi
say "   证书指纹：$FP"
say ""
say " 还要做两件事："
say " 1) 阿里云控制台 → 这台 ECS → 安全组 → 入方向 → 放行 TCP ${PORT}"
say "    （来源建议填你常用网络的出口 IP；图省事可 0.0.0.0/0，但 Basic 凭据只靠 TLS 保护）"
say " 2) RClaw → 多端同步 → 创建同步空间 → WebDAV：填上面的地址/用户名/密码"
say "    自签证书要先点「读取证书」→ 核对指纹与上面一致 →「核对过了，信任这张证书」"
say "    然后「测试连接」六项全过，再创建空间"
say ""
say " 其它设备加入时：创建端在「添加设备」显示配对码；若配对码不带存储设置，"
say " 在本机填同样的地址/账号，并各自核对一次证书指纹（每台设备都要固定一次）。"
say " 有域名时建议改用受信任证书（RCLAW_DOMAIN/RCLAW_CERT/RCLAW_KEY），就不用每台设备固定指纹。"
say "================================================================"
