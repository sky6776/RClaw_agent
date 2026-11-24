# 多端同步：在第三方服务器上部署文件服务器（WebDAV）

> 面向"自己有一台服务器 / NAS / 网盘，想让 RClaw 在多台设备之间同步"的部署与使用说明。
> 服务端只需要是一个 **WebDAV 文件服务**，不需要安装 RClaw；RClaw 写上去的同步对象都是
> 密文，服务端看不到会话内容、记忆内容或文件内容（两个空间标记是明文 JSON，见第 2 节）。

## 1. 服务器需要满足什么

RClaw 日常同步用到这些操作（没有 HEAD、MOVE、锁或批量接口依赖）：

| 操作 | 用途 |
|---|---|
| `PROPFIND`（`Depth: 1`） | 列目录 |
| `PROPFIND`（`Depth: 0`） | 取对象长度（写后核对） |
| `GET` / `PUT` / `DELETE` | 读写、删除对象 |
| `MKCOL` | 创建目录（客户端会按需补建） |
| `If-None-Match: *`（条件 PUT） | "仅创建"空间标记；服务端不实现时见下 |

要点：

- **目录列表和对象长度查询都要能通过**：代理必须原样转发 `Depth` 头，DAV XML 返回里
  每个文件要带 `getcontentlength`，否则列目录/写后核对会失败。
- **公网地址必须 HTTPS**。局域网地址可以用 `http://`，但要在客户端显式勾选"允许不加密的
  http 连接"（见第 8 节：账号和密码在明文 HTTP 里是裸传的）。
- 账号用 **Basic Auth**，优先用应用专用密码（网盘支持时），不要用主账号密码。
- 一个**专用目录**（例如 `/srv/rclaw-sync/` 或网盘里的 `/rclaw/`），不要和别的数据混放。
- **云服务器（阿里云 ECS / 腾讯云 CVM 等）还要放行云平台的安全组**：只开机器里的
  firewalld/ufw 不够；阿里云的具体路径见第 4 节的「阿里云安全组」。
- 反向代理：保留 DAV 方法、`Authorization`、`Depth`、`If-None-Match` 和请求体；**同步路径
  要绕过缓存（包括读缓存）**，`heads/` 和两个空间标记是会被反复改写的；请求体上限要大于
  你打算同步的最大对象（密文比原文略大：默认单附件 50 MiB 原文，见第 9 节）。
- **条件创建的真实语义**：客户端创建 `space.json` 时总是发 `If-None-Match: *`。
  - 服务端实现了它：对象已存在时条件 PUT 返回 **412**，原内容保持原样。
  - 服务端忽略它、按普通 PUT 覆盖：客户端靠独立的关闭墓碑 `space-closed.json` 保留关闭
    判定（旧版客户端只认兼容标记，仍可能受迟到创建影响，请先升级 RClaw）。
  - 服务端直接拒绝条件请求：客户端会读回并核对已有标记；目标原本不存在、请求又被拒绝时，
    创建会失败。
  - 需要服务端防覆盖时，用临时对象验收：首次 `PUT` + `If-None-Match: *` 成功，第二次写
    同一对象返回 412、读回仍是第一次的内容。**「测试连接」不覆盖这一步**；同样的双写
    验收也用于判断一个对象存储能否作为「一致性要求高」的落点。
- 点「读取证书」时会对同源的 `/` 发一个**不带凭据**的 `OPTIONS`，只为拿到 TLS 握手证书，
  不参与日常同步。
- 不要求服务端一直在线：设备同步时能连上即可；连不上会退避重试（见第 9 节）。

## 2. 目录与账号规划

```
/srv/rclaw-sync/                 <- 给 RClaw 的专用目录（WebDAV 根）
└── <space_id>/                  <- RClaw 自己建的空间目录
    ├── space.json               <- 明文：format / space_id / check / closed
    ├── space-closed.json        <- 空间废弃墓碑（明文，重置后出现）
    ├── heads/<device_id>        <- 每设备的 head（密文）
    ├── devices/<device_id>/ops/*.seg       <- 段（密文）
    ├── devices/<device_id>/rollups/*.pack  <- 汇总包（密文）
    ├── blobs/<前两位>/<blob_id>            <- 附件（密文）
    └── snapshots/<device_id>/*.snap|*.part <- 全量快照（密文）
```

- 同步内容对象都是密文；`space.json` / `space-closed.json` 是**明文 JSON**，字段为
  `format`、`space_id`、`check`（密钥校验值）、`closed`。服务端还能看到对象名、大小和
  时间等元数据。
- 「测试连接」会在存储根的 `selftest/` 下试写临时对象；测试对象会尽力删除，但
  `selftest/` 目录本身会留着（传输层没有删目录的操作）。
- **各设备指向同一个逻辑存储根即可**：地址可以各自可达（同一域名/端口通常最省事，但
  NAT 回环、内外网双入口时允许不同），匹配空间看的是存储根、`space_id` 和密钥校验值；
  凭据只要有权操作该根。所用账号（或每台设备各自的账号）都要能在该目录**列目录、读取、
  建目录、写入、删除**。
- 网盘有"应用密码"时优先用它；自建服务用权限仅覆盖同步目录的专用账号。

## 3. 方案 A：rclone serve webdav（最快上手）

先用 htpasswd 建专用账户（bcrypt），前台命令与 systemd 共用这个文件，不要把密码写进
命令行或单元文件：

```bash
htpasswd -cB /etc/rclaw-webdav.htpasswd rclaw     # 交互输入密码；以后改密码去掉 -c
mkdir -p /srv/rclaw-sync
rclone serve webdav /srv/rclaw-sync \
  --addr 0.0.0.0:8443 \
  --htpasswd /etc/rclaw-webdav.htpasswd \
  --cert /etc/letsencrypt/live/sync.example.com/fullchain.pem \
  --key  /etc/letsencrypt/live/sync.example.com/privkey.pem
```

systemd 单元（`/etc/systemd/system/rclaw-webdav.service`）：

```ini
[Unit]
Description=RClaw WebDAV
After=network-online.target
Wants=network-online.target

[Service]
User=rclaw-webdav
ExecStart=/usr/bin/rclone serve webdav /srv/rclaw-sync --addr 0.0.0.0:8443 \
  --htpasswd /etc/rclaw-webdav.htpasswd \
  --cert /etc/letsencrypt/live/sync.example.com/fullchain.pem \
  --key /etc/letsencrypt/live/sync.example.com/privkey.pem
Restart=always
RestartSec=3

[Install]
WantedBy=multi-user.target
```

- 建一个专用系统用户 `rclaw-webdav`：给它 `/srv/rclaw-sync` 的写权限、**遍历证书路径**并
  读取证书链和私钥的权限（私钥只开放给该服务的账号或组），以及**读取认证文件**的权限——
  建议用专用组：认证文件设成 `root:rclaw-webdav`、`0640`，把系统账号加进该组；启动前用
  `sudo -u rclaw-webdav test -r /etc/rclaw-webdav.htpasswd` 确认。漏掉这一步时前台以
  root 跑能认证，切到 `User=` 后同样的密码会变成 401。系统级 unit 默认以 root 运行，
  别省 `User=`。
- `--addr 0.0.0.0:8443` 只监听 IPv4 所有接口（直接对外用，记得放行端口）；只给反向代理
  用就改成 `127.0.0.1:8443`；要 IPv6 直连就另配 `--addr '[::]:8443'`（或以代理提供
  IPv6 入口）。客户端地址要带实际域名与端口，例如 `https://sync.example.com:8443/`。
- 第一次启用 systemd：先停掉前台 rclone，再 `sudo systemctl daemon-reload &&
  sudo systemctl enable --now rclaw-webdav.service`。
- 用 Certbot 时，续期后要重新加载证书：在续期成功的 **deploy hook** 里执行
  `systemctl restart rclaw-webdav.service`（rclone 不会自己重读证书；重启会短暂断开同步，
  客户端会退避重连）。
- **已知限制**：rclone 的 WebDAV `PUT` 把 `If-None-Match` 当普通覆盖处理（不实现条件
  创建）。RClaw 用独立的关闭墓碑兜底，正常使用不受影响；但只认兼容标记的旧版客户端在
  "重置同步空间"与另一台设备迟到创建并发的极端情况下可能仍看到旧空间。对一致性要求高的
  场景请用 Apache（下一节），或通过第 1 节双写验收的对象存储。

## 4. 方案 B：Apache httpd + mod_dav（最标准，条件创建完整）

> 阿里云 ECS（Alibaba Cloud Linux / Ubuntu / Debian）：仓库里的
> `scripts/deploy-rclaw-webdav-aliyun.sh` 配置 Apache WebDAV——装包、建目录与账号、自签证书、
> SELinux/firewalld 都做完。**首次建账号或提供 `RCLAW_PASS` 时**会跑本机自检（PROPFIND、
> MKCOL、条件创建双写 + 读回）；输出「未自检」时，按第 1 节手动补做双写和读回验收。
> 之后用脚本打印的地址、账号和证书指纹配置客户端；脚本头注释里有可选环境变量
> （`RCLAW_ALLOW_IPS` 会自动放行本机回环，客户端来源要自己填）。
>
> **阿里云安全组**（脚本跑完必须做，否则外网连不上）：控制台 → `ECS` → 实例 → 该实例的
> 「安全组」→「配置规则」→ 入方向 → 手动添加：协议 `TCP`、端口范围 `8443/8443`（脚本改了
> `RCLAW_PORT` 就填对应端口）、授权对象填**各客户端的可信公网出口 IP/CIDR**（单个地址写
> `x.x.x.x/32`）。出口 IP 经常变（换网络、运营商 NAT）时，按第 10 节用私有组网，不要图省事
> 开 `0.0.0.0/0`——那等于把这个只靠 Basic 认证的私人 WebDAV 暴露给全网。
> 云服务器是**两层**防火墙：云平台安全组 + 机内 firewalld/ufw，两边都要放行。

```apache
LoadModule dav_module      modules/mod_dav.so
LoadModule dav_fs_module   modules/mod_dav_fs.so
LoadModule ssl_module      modules/mod_ssl.so

# DAV 锁数据库：必须在导出目录之外，运行用户可写
DavLockDB /var/lib/rclaw-dav/DavLock

<VirtualHost *:443>
    ServerName sync.example.com
    SSLEngine on
    SSLCertificateFile    /etc/letsencrypt/live/sync.example.com/fullchain.pem
    SSLCertificateKeyFile /etc/letsencrypt/live/sync.example.com/privkey.pem

    Alias /rclaw /srv/rclaw-sync
    <Directory /srv/rclaw-sync>
        Dav On
        Options Indexes FollowSymLinks
        AuthType Basic
        AuthName "RClaw Sync"
        AuthUserFile /etc/apache2/rclaw.htpasswd
        Require valid-user
        # DAV 的 XML 请求另有 LimitXMLRequestBody 上限；同步对象作为 PUT 请求体传输，
        # 不归它管。对象上传的有效上限请在实际 Apache 版本与所有代理上核对（见第 1、9 节）。
    </Directory>
</VirtualHost>
```

- 先建好 `/var/lib/rclaw-dav` 和 `/srv/rclaw-sync`，让 Apache 的运行用户（Debian/Ubuntu
  上是 `www-data`）可写这两个目录。发行版若已配置可写的 DAV 锁库，可沿用。
- 创建账号：`htpasswd -c /etc/apache2/rclaw.htpasswd rclaw`。
- 客户端地址填：`https://sync.example.com/rclaw/`（末尾斜杠会自动补齐，见第 9 节）。
- Apache 2.4 的 mod_dav 实现了 `If-None-Match` 条件创建，是这里条件语义最完整的方案。

## 5. 方案 C：Caddy（自动 HTTPS）+ webdav 插件

`webdav` 指令来自第三方插件，需要带插件构建，并确认实际运行的二进制是 Caddy 2.8+：

```bash
xcaddy build --with github.com/mholt/caddy-webdav
```

```caddyfile
sync.example.com {
    root * /srv/rclaw-sync
    route {
        basic_auth {
            rclaw BCRYPT_HASH
        }
        webdav
    }
}
```

先运行 `caddy hash-password --algorithm bcrypt` 交互输入密码，然后把上面那行里的
`BCRYPT_HASH` **整个替换**为命令输出的一整行哈希（保留输出自带的前缀，不要再加
`$2a$14$`）。占位符不能带空格或说明文字，否则 Caddy 会把它们当成多余参数拒绝加载。

- 预建 `/srv/rclaw-sync` 并授予 Caddy 运行用户读写权限；插件从 `root` 取目录。
- 公网证书要求正确的 A/AAAA 记录和可达的 ACME 验证入口（HTTP-01 用 TCP 80，
  TLS-ALPN-01 用 TCP 443）。不方便开放验证入口时用 DNS-01：`xcaddy` 除了
  `github.com/mholt/caddy-webdav` 还要加入对应的 `caddy-dns` provider 模块，在站点的
  `tls` 块按该模块文档配置 `dns <provider> ...`，并让运行中的 Caddy 能取得有权修改
  挑战记录的 DNS 凭据；也可以直接部署一张已签发、客户端能验证的证书。
- **已知限制**：该插件的 `PUT` 同样委托给 Go 的 WebDAV 实现，`If-None-Match` 被当成普通
  覆盖（与 rclone 相同的限制），一致性要求高时用 Apache，或用通过第 1 节双写验收的对象存储
  （阿里云 OSS 勾选 OSS 开关后可用，见第 6 节）。

## 6. 方案 D：现成网盘 / NAS

| 服务 | 客户端填的地址 | 备注 |
|---|---|---|
| 坚果云 | `https://dav.jianguoyun.com/dav/<目录>/` | 用"应用密码"；先查当前套餐的请求频率、月流量、单次列表条数与上传上限，首次同步可能耗用较多配额 |
| Nextcloud | `https://host/remote.php/dav/files/<用户名>/<目录>/` | 用应用密码；部署在子路径时保留安装路径 |
| AList | `https://host/dav/<挂载路径>/` | 确认驱动支持相关操作，并按版本给账号开放 WebDAV 读取与管理、建目录/上传、删除权限 |
| 群晖 WebDAV Server | `https://host:5006/<共享文件夹>/` | 套件里开启 HTTPS（默认 HTTPS 5006 / HTTP 5005，以套件实际设置为准）；账号要获准使用套件并对共享文件夹有读写权限 |
| 阿里云 OSS / 腾讯 COS / R2 / MinIO | —— | 走 `s3` 后端，不是 WebDAV；见下 |

**对象存储（S3 兼容）**：RClaw 固定使用 **AWS SigV4**、**ListObjectsV2**、
GetObject / PutObject / DeleteObject，以及创建空间用的条件 `PutObject`
（`If-None-Match: *`）。按厂商文档填 S3 API 服务地址、区域、桶名与专用访问密钥：

- **阿里云 OSS：在 S3 表单里勾选「阿里云 OSS（用 x-oss-forbid-overwrite 防覆盖）」。**
  OSS 的 `PutObject` 会直接拒绝标准条件头（`If-None-Match` 等返回 `400
  NotImplemented`，官方错误码 0017-00000245），所以 RClaw 在这个模式下改用 OSS 专有的
  `x-oss-forbid-overwrite: true` 做「仅创建」（已存在返回 409 `FileAlreadyExists`）。
  按下面填：
  1. **服务地址**：`https://s3.oss-cn-hangzhou.aliyuncs.com`（换成你的地域；这是 **S3
     兼容端点**，不是普通 OSS 端点，也不要带桶名或路径）。设备在**同地域 ECS** 上时可换
     `-internal` 端点，但不经过 ECS 的客户端用不了内网端点。
  2. **区域**：填**区域 ID** `cn-hangzhou`（不是端点里的 `oss-cn-hangzhou`）。RClaw 把它
     原样写进 SigV4 的签名范围，填错会 `SignatureDoesNotMatch`。
  3. **桶**：`从未开启/暂停过版本控制`——开启过之后 `x-oss-forbid-overwrite` 会失效
     （允许覆盖），「仅创建」就不成立了。**这是 OSS 上线的硬条件**。
  4. **不要勾**「路径风格的地址」：OSS 只支持虚拟主机式。
  5. **账号**：先确认该账号能用 S3 兼容的 V4 签名（未开通时请求报 `0002-00000033`，
     要联系阿里云技术支持开通）。
  6. **权限**（RAM 子账号，最小集）：`oss:ListObjects` 授在**桶** ARN 上、用条件
     `oss:Prefix` 限制到桶内目录（例如 `rclaw/*`）——RClaw 是在桶路径上带 `prefix` 列举的，
     只授对象前缀会「列目录」失败；对象 ARN `<bucket>/rclaw/*` 授 `oss:GetObject` /
     `oss:PutObject` / `oss:DeleteObject`，范围要覆盖 `selftest/` 和空间对象。
  7. **2025-03-20 之后开通的账号**：访问**中国内地桶的默认公网域名**做数据 API 会被拒
     （`400 PublicEndpointForbidden`）。这时要用自定义域名，而且注意 RClaw 会按虚拟主机式
     **在填写的域名前再拼桶名**：桶 `rclaw-sync`、填 `https://oss.example.com`，实际请求的是
     `https://rclaw-sync.oss.example.com`——OSS 绑定、CNAME 和证书都要按这个主机名配置。
  8. **版本**：OSS 空间的所有设备都要用**包含该开关的构建**（当前分支自 `ddf28d6` 起；
     正式版从下一个包含它的发布开始）——`v2.8.34` 的正式包早于这个功能，装它仍会发
     `If-None-Match` 被 400 拒绝。不勾这个选项时，OSS 会让**创建空间直接失败**（不是退化成
     墓碑兜底）。
  验证：在隔离前缀下跑手动验收（会写一个几十字节的对象再删）：

  ```bash
  # 先按实际配置导出（值用引号，不要留 <> 尖括号：shell 会当成重定向）
  export RCLAW_OSS_ENDPOINT="https://s3.oss-cn-hangzhou.aliyuncs.com"
  export RCLAW_OSS_REGION="cn-hangzhou"
  export RCLAW_OSS_BUCKET="你的桶"
  export RCLAW_OSS_ACCESS_KEY_ID="你的AK"
  export RCLAW_OSS_SECRET_KEY="你的SK"
  # 探针目录要落在 RAM 授权的前缀里：改成你的「桶内目录」（ROS 模板输出的 Prefix，
  # 或手填时与 RClaw 表单里的桶内目录一致）
  rclaw_ros_prefix='rclaw'
  export RCLAW_OSS_PREFIX="${rclaw_ros_prefix}/acceptance-$(date +%s)-$$"

  RCLAW_OSS_PATH_STYLE=0 RCLAW_OSS_FORBID_OVERWRITE=1 \
    cargo test -p rclaw-sync --test oss_conditional -- --ignored --nocapture
  ```

  通过的标准是：第一次写入成功；第二次被拒、状态码是 **409** 且错误码是完整的
  `FileAlreadyExists`；读回仍是第一次的内容。桶开过版本控制时第二次会成功，验收会明确失败。
  默认公网端点被限制时，把 `RCLAW_OSS_ENDPOINT` 设成 RClaw 表单里填的**不带桶名**的地址
  （按本节第 7 点）：桶 `rclaw-sync`、填 `https://oss.example.com`，并为实际请求主机
  `rclaw-sync.oss.example.com` 配置 OSS 域名绑定、CNAME、证书（内地还需备案），再跑同一套
  验收命令——不要直接把已绑定的桶域名填进 `RCLAW_OSS_ENDPOINT`（虚拟主机式会再加一次桶名）。
- **一键创建 OSS（ROS 模板）**：仓库里的 `scripts/aliyun-oss-rclaw-stack.yml` 是资源编排
  模板，导入后自动建好：私有桶（**不开启版本控制**、阻止公共访问）、RAM 子账号、最小权限
  策略（`oss:ListObjects` 授桶 + `oss:Prefix` 限制到目录；目录下对象读写删）、以及该子账号
  的 AccessKey。

  前置条件（与模板是否成功无关，但要先确认）：账号已开通 OSS 的 S3 兼容 V4 签名（报
  `0002-00000033` 要联系技术支持）；默认公网端点在 2025-03-20 之后新开通 OSS 的账号上访问
  中国内地桶可能被拒（`PublicEndpointForbidden`），此时用自定义域名，或从同地域可访问 OSS
  内网的 VPC 内客户端（ECS/函数计算等）用 `OssInternalEndpoint`。

  1. 在 ROS 控制台选好**目标地域**（桶建在那里），进入「资源栈 → 创建资源栈 → 选择已有模板
     → 输入模板」，把模板内容粘贴到 ROS 页签。
  2. 填**资源栈名称**和全局唯一的 `BucketName`；`Prefix`、`UserName`、`PolicyName` 默认
     `rclaw`、`rclaw-oss`、`rclaw-oss-sync`（同账号里重名就换一个）。
  3. 确认当前部署身份（或栈执行角色）有权：建 OSS 桶（`oss:PutBucket`）、开桶级阻止公共
     访问（`oss:PutBucketPublicAccessBlock`）、创建 RAM 用户/自定义策略/策略关联、创建
     AccessKey；在「检查并确认」页点「预览模板资源」（或 `ValidateTemplate`）再创建。
  4. 创建完成后在「输出」里复制 Bucket / Endpoint / Region / Prefix / AK。**这个输出里有
     长期有效的 AccessKey Secret**——能查询这个资源栈的 RAM 身份都能看到它（包括拥有
     `AliyunROSReadOnlyAccess` 的人），别把输出发到聊天/工单/截图/仓库。
  5. **先把模板生成的那对密钥换掉**：在 RAM 控制台给这个子账号再新建一对 AccessKey，
     并确认新密钥可用。**停用旧密钥要满足条件**：没有设备还在用旧密钥时才禁用/删除；
     已有设备用旧密钥的，先把各设备切到新密钥、确认同步正常，再禁用旧的。**填表单和跑
     验收都用新密钥**——模板输出里的 AK 只当作"拿到这个子账号"的过渡，不要长期使用。
     怀疑泄露时立即禁用（不等设备切换）。
  6. 用管理身份核对：OSS 控制台该桶「版本控制=未开启、读写权限=私有、阻止公共访问=已开启」；
     RAM 里策略已附加给新用户、内容只有上面那几项权限。
  7. 回到本节**上方**的「阿里云 OSS」使用说明与双写验收命令：勾「阿里云 OSS」，用**第 5 步
     的新 AccessKey ID/Secret** 填表单；探针前缀用**你这个栈的 `Prefix` 输出**（命令里的
     `rclaw_ros_prefix`）。
  > 这个模板只在本地做过 YAML 结构/字段检查，**没有在真实账号里执行过**，也没有经过 ROS 的
  > 资源属性校验；请按第 3 步先预览/校验，再按第 7 步完成六项自检、空空间创建/加入与双写
  > 验收。
- **COS**：2024-01-01 之后创建的桶必须关闭路径式寻址，更早的桶也推荐关闭。
- 自检通过不等于条件写可用：先用同一套配置**实际创建并加入一个空空间**，再按第 1 节做
  双写读回验收（创建/加入只证明条件请求被接受，不证明已有对象不会被覆盖；OSS 的完整验收
  命令见上）。

## 7. RClaw 端配置（各设备按需）

第一台设备（创建）：

1. 打开「多端同步」→「创建同步空间」。
2. 存储类型选 **WebDAV**：填「地址」「用户名」「密码」；局域网 `http://` 勾选"允许不
   加密的 http 连接"（勾选前先看第 8 节的凭据风险）。
   选 **S3**：先在云端建好桶。「服务地址（Endpoint）」填**不带桶名、不带路径**的 S3 API
   端点（例如 `https://s3.example.com:9000`）；再填「区域（Region）」「桶名」「桶内目录
   （可不填）」「Access Key ID」「Secret Access Key」；按实际后端设置「路径风格的地址
   （path-style）」（用 IP 作服务地址时勾上）。**阿里云 OSS 还要勾选「阿里云 OSS（用
   x-oss-forbid-overwrite 防覆盖）」，并且不要勾路径风格**（OSS 只支持虚拟主机式；桶也不能
   开启过版本控制，见第 6 节）。局域网 `http://` 的 S3（如 MinIO）也要在 S3 表单里勾选
   「允许不加密的 http 连接」，并满足第 8 节的地址范围——不勾会报「公网地址必须使用 HTTPS」。账号权限至少要能桶级列举加对象的读、写、删
   （AWS：`s3:ListBucket`、`s3:GetObject`、`s3:PutObject`、`s3:DeleteObject`）；如果限制到
   某个前缀，该根下的 `selftest/` 和空间对象都要放行。
3. 点「测试连接」。成功时依次显示六项：**建目录并写入、列目录、读回比对、覆盖写、删除、
   上传 2 MB 的对象**（实际 2 MiB）。配置本身不合法时只显示「连接设置」；操作失败时先处理
   第一个失败项，后面的不再做。六项通过后仍建议实际创建并加入一个空空间，并按计划使用的
   上传大小再验收一次。
4. 「创建同步空间」→ 在「多端同步 → 添加设备」里点「显示配对码」（可显示二维码）。
   WebDAV 和 S3 都默认把存储地址、账号、密码和相关连接选项装进配对码，可以取消"配对码里
   带上存储的地址和密码"；取消后另一台设备要自己填存储设置（WebDAV 含明文开关、证书指纹，
   S3 含区域/桶/前缀/路径风格）。配对码等同于全部同步数据的钥匙：不要截图、不要经聊天
   软件转发，粘贴用完清空剪贴板。

其余设备（加入）：

1. 「多端同步」→「加入同步空间」。
2. 粘贴配对码到「配对码」输入框，或「从图片识别」一张二维码图片；Android 还可以「拍照
   扫码」。**配对码自带存储设置时，先核对界面显示的地址/桶是不是你要连的那个**（配对码
   来自谁，地址就是谁填的）。码里不带存储设置时，按创建时的字段在本机填：WebDAV 填
   地址、账号及适用的证书/明文设置；S3 填服务地址、区域、桶名、桶内目录、密钥、路径风格
   和适用的明文设置。
3. 加入后通常先拉取并合入可用的远端内容，再发布本机存量。如果新设备一直没看到其他设备的
   日志，首次发布可能先等待约 10 分钟（防止把"别处已有的旧样子"当存量再发出去）。
4. 首次同步优先用非计费网络；Android 在计费网络上的首次实际同步会要求确认。开启附件后，
   界面若问已有附件，按需选全部、最近 30 天或都不上传。

各设备在「同步哪些内容」里独立选择：会话、记忆与每日笔记、人格、附件、技能、智能体定义、
定时任务。附件需要同时开启会话；定时任务默认在原设备执行，不会因为同步直接在另一台设备
运行。配置里的 API Key、设备全局权限和 `rclaw.toml` 永远保留在本机；智能体定义里的工具
限制会随定义同步，放宽限制时要在本机确认。

## 8. TLS 与证书

- 公网：用受信任证书（Let's Encrypt / 商业证书）。RClaw 走系统与内置的公共 CA。
- **自签证书**：先从服务器控制台或可信管理员处**带外取得**叶证书的 SHA-256 指纹，再在
  RClaw 点「读取证书」，核对界面显示的指纹与你手上的完全一致，才点「核对过了，信任这张
  证书」。信任之后只认这一张证书，证书换发后各设备要重新核对。只从连接结果抄指纹再信任
  会漏掉独立核对这一步。
- 普通 HTTPS 校验失败时，检查域名、有效期和完整证书链；对象存储没有 WebDAV 的指纹选项，
  必须用客户端信任的证书链。
- 明文 HTTP 只允许**解析结果全部**属于这些地址：IPv4 `10/8`、`172.16/12`、`192.168/16`、
  `127/8`、`169.254/16`；IPv6 `::1`、`fc00::/7`、`fe80::/10`；IPv4 映射 IPv6 按内嵌
  IPv4 判定。主机名可用，但解析出的每个地址都要合规。
  **`100.64/10`（CGNAT，Tailscale 常用网段）不算私网**——组网方案请看第 10 节。
- 即使填的是私网地址，**没勾选**"允许不加密的 http 连接"也会收到"公网地址必须使用 HTTPS"
  提示。勾选后 Basic 的用户名和密码在请求里是明文过网的：只在可信局域网或受控加密组网
  里使用。
- 凭据只随**同源**（协议 + 主机 + 端口）请求发送。跨源的 `GET` 下载会改用不带 WebDAV
  凭据的 HTTPS 客户端重新取（要求目标允许匿名访问并通过默认 TLS 校验）；跨源写入、列目录
  和其他 DAV 操作不会被跟随——请让代理把请求直接送到后端。

## 9. 常见问题

| 现象 | 原因与处理 |
|---|---|
| 测试连接 401 / 403 | 用户名或应用密码不对；账号没有该目录权限；服务端禁了 `PUT`/`PROPFIND`。按失败步骤检查 Basic 凭据与对应目录权限 |
| 测试连接 404，或"列目录里没有刚写入的对象" | 地址不是刚建的专用目录，或代理没把 DAV 路由到后端。客户端会自动补末尾斜杠，手补 `/` 不解决 404 |
| 测试连接 409 / "上级目录不存在" | 父目录缺失：客户端只补建有限层级。先在服务端建好存储根与父目录，再确认 `MKCOL`/`PUT` 送到正确后端 |
| OSS 报 `0002-00000033` | 账号没开通 S3 兼容的 V4 签名：联系阿里云技术支持开通，或改用 Apache WebDAV |
| OSS 报 `PublicEndpointForbidden` | 2025-03-20 之后开通的账号访问内地桶的默认公网域名受限：用自定义域名（按 `<bucket>.<域名>` 绑定并配证书），或在同地域 VPC 内用 `-internal` 端点 |
| OSS 报 `NotImplemented`（400） | 没勾「阿里云 OSS」开关，客户端发的仍是 `If-None-Match`（见第 6 节） |
| OSS 上第二次写同一对象仍成功 | 桶开启/暂停过版本控制，`x-oss-forbid-overwrite` 失效；换一个从未开过版本控制的桶 |
| 提示"这个地址不支持 WebDAV（服务端拒绝了 PROPFIND），请检查地址" | 路由不对，或服务端缺 DAV 扩展模块（如 nginx 自带 dav 模块不支持 `PROPFIND`，换 Apache / Caddy / rclone） |
| 提示"公网地址必须使用 HTTPS" | 没勾选允许明文，或指定 IP / 域名的**任一 DNS 解析地址**超出第 8 节允许范围。改用 HTTPS，或让地址及所有解析结果符合第 8 节并勾选允许明文 |
| 提示证书错误 | 自签证书没带外核对并信任指纹，或指纹已变；公网证书检查域名/有效期/证书链 |
| 大附件传不上去 / HTTP 413 | 服务端、反代或隧道的请求体上限按**密文对象**计（默认单附件 50 MiB 原文）且要覆盖汇总包；报告 413 时查整条链路的上限。也可能是超过 RClaw 本地上限 |
| 上传停滞或一直等响应 | 先分清三种：**建连**失败约 20 秒超时；上传对象没有固定总时长，但**进度停滞约 60 秒**判超时（大文件慢网络不受总时长限制）；请求体交完后等响应的时限随对象大小增加、**最多 15 分钟**。分别检查网络/端口、上传链路上的服务端落盘与代理超时、后端响应 |
| 重置后旧设备还能打开旧空间 | 条件创建被服务端忽略：rclone / Caddy 插件都有这个限制，改用 Apache，或用通过第 1 节双写验收的对象存储；并确认旧设备已升级 RClaw。OSS 要勾选「阿里云 OSS」开关，否则会被 400 拒绝（见第 6 节） |
| 同步偶尔慢/停 | 服务端不可达时会退避重试：普通网络错误从 5 秒起指数退避、最长 5 分钟；429/503 按 `Retry-After` 处理——同步轮把服务端给的时长限制在 **5 秒至 15 分钟**，没给出有效值时按普通退避。恢复后不会收到推送，靠后续重试或下一次同步触发继续。前台默认每 60 秒轮询；Android 后台默认每 15 分钟且只在非计费网络做定时轮询。证书、权限或地址错误要先把配置改对 |

## 10. 没有公网 IP 怎么办（家庭电脑 / NAS / 手机）

存储服务器可以是家里的电脑、NAS 甚至手机，只要**同步时**设备能连上它。无论选下面哪种
方式，**每台设备都要从它实际使用的网络完成六项自检**（家里 Wi-Fi、公司网、蜂窝网各验
一次，出口不同可能被防火墙或运营商挡）。自检入口：用**不带存储设置**的配对码时，加入页
填好存储后可以直接点「测试连接」；用**带存储设置**的配对码时，加入页只显示地址和加入
按钮，加入后到「多端同步 → 存储 → 修改」点「测试连接」，验完点「取消」即可（不改配置），
要加入前就验收就改用不带存储设置的配对码。

常见做法：

1. **组网（推荐）**：Tailscale / ZeroTier / WireGuard 把设备放进同一个虚拟局域网。
   - WireGuard / ZeroTier 的虚拟网段由实际配置决定（常见是 `10.x` / `172.16-31.x`），
     选与现网不冲突的网段；组网后填服务真实可达的虚拟 IP。裸 WireGuard 还需要一个对端
     可达的 UDP 入口（公网端口映射或中继）。
   - Tailscale 的 `100.64/10` **不在** RClaw 的私网判定里：在 Tailscale 里启用 MagicDNS
     与 HTTPS，用 `Serve` 把本地 DAV 代理到 `https://<机器>.<tailnet>.ts.net/`，或直接给
     DAV 监听器部署该证书。只有 DAV 监听 Tailscale 的 ULA IPv6 且两端可达时，才能用其
     字面地址配合明文开关。
2. **隧道**：Cloudflare Tunnel / frp / ngrok 把本机 WebDAV 映射到公网域名。
   - 不需要公网 IP；注意隧道对请求体大小、超时和并发的限制（Cloudflare 免费档单请求
     100 MB，且站点可以调低）。
   - **额外访问控制必须对客户端透明**：普通 RClaw 请求无法完成浏览器登录、验证码或
     Cloudflare Access 服务令牌那种专用请求头认证。保留专用 Basic Auth，再加限制时用
     私有组网或来源 IP 规则这类网络层手段。
   - frp 需要一台可公网访问的 `frps` 作中继，对客户端提供 HTTPS。
3. **IPv6**：家里宽带和手机蜂窝都有 IPv6 时用直连——但要同时满足三点：**服务端有公网
   可路由的 IPv6 地址、DAV（或代理）监听该地址、客户端到该地址实际可达**；再配 AAAA
   记录 + DDNS + 防火墙放行端口，并且仍然用 HTTPS。只有"有 IPv6"不足以确认这三项。
4. **路由器端口映射**：需要公网 IPv4；先检查实际 WAN 地址是否可入站（不少家庭宽带是
   CGNAT，拿不到）。同一域名在内外网都用时，两边都要验证实际可达性。
5. **手机当服务器**：临时可用（Android 上的 WebDAV Server 类应用 + 组网），但受 Doze、
   省电和后台限制，不适合长期。长期方案用 NAS / 迷你主机 7×24。

注意：家里电脑睡眠/关机时同步会暂停（客户端退避重试，恢复后继续）；动态 IP 用 DDNS 或
组网工具解决。

## 11. 部署检查清单

- [ ] WebDAV 目录专用、可写，父目录存在；运行用户对该目录和 DAV 锁库有写权限
- [ ] 代理保留 DAV 方法、`Depth`、`Authorization`、`If-None-Match` 和请求体；同步路径
      绕过缓存（含读缓存）
- [ ] 目录列表和对象长度查询都能通过（`Depth: 1/0`，DAV XML 带 `getcontentlength`）
- [ ] HTTPS 证书有效（或已**带外核对**并固定 WebDAV 证书指纹）
- [ ] 账号权限：WebDAV 专用账号 / 应用密码能列目录、读取、建目录、写入和删除；S3 访问
      密钥有桶级 `s3:ListBucket`——若用 `s3:prefix` 限定，要允许 `selftest/` 和空间目录下
      各子前缀（桶内目录为 `rclaw` 时可用 `StringLike` 匹配 `rclaw/*`，只放行目录本身会在
      自检「列目录」处 403），同一范围内的 `selftest/` 对象和空间对象可读、写、删
- [ ] 请求体上限按密文对象核对，并覆盖汇总包；按计划上限实际传过一个大对象
- [ ] 条件创建已验收（第二次 `PUT If-None-Match: *` 返回 412、读回仍是第一次的内容；
      OSS 模式下对应的是 409 `FileAlreadyExists`），或已知服务端不实现并接受墓碑兜底的边界
- [ ] 用 OSS 时：勾选「阿里云 OSS」开关、桶从未开启过版本控制、未勾路径风格；服务地址是
      S3 兼容端点、区域填 `cn-hangzhou` 这种区域 ID；账号已开通 S3 V4 签名；RAM 授权
      `oss:ListObjects`（桶 ARN + `oss:Prefix`）与对象读写删；所有设备用**包含该开关的构建**
      （当前分支自 `ddf28d6` 起；`v2.8.34` 正式包没有它）；在授权前缀下跑双写验收（409 +
      `FileAlreadyExists`、读回第一次内容）
- [ ] 防火墙只放行需要的端口；公网暴露时限制来源 IP。云服务器要放行**两处**：云控制台的
      安全组（如阿里云 ECS 的 入方向规则）和机内 firewalld/ufw
- [ ] 在 RClaw 里「测试连接」六项全过，并实际创建 / 加入一个空空间
- [ ] 第一台设备创建空间后，配对码只在自己设备之间传递
- [ ] 各设备同步范围按需选择（附件/技能/智能体等），附件记得同时开启会话
- [ ] 各设备启用操作系统自动校时（时钟偏差会影响"后写者胜"，S3 签名也用本机时间）
- [ ] 服务端目录有备份策略，按空间状态分别对待：
      - **关闭前的空间**：用同一时间点的存储快照备份，或在所有写入和清理都暂停时完整复制
        （普通文件级复制可能跨越 `heads/` 的更新、漏掉它引用的新对象）。按 space_id 单独
        保存对应的配对码——**不带存储密码**、离线或加密保存，并记下恢复位置和登录方式。
      - **已关闭的空间**（`space.json` 的 `closed: true` 或存在 `space-closed.json`）：
        即使密钥匹配，加入也会返回「空间已关闭」；这类副本只作归档，别删标记、别改
        `closed`，也别把它覆盖回生产根。
      - **恢复演练**：在隔离、可写的存储根还原**关闭前**的快照（保留 `<space_id>/` 层级），
        用**空白本机数据根**启动的客户端（避免本机原有数据掩盖缺件）指向它的父根，用
        **不带存储设置**的配对码加手填地址「加入」；到「同步哪些内容」**开启「会话」与
        「附件」**（干净客户端默认附件关闭），等拉取完成后抽查一个备份前已知同步完成的
        会话，并**实际下载、打开**它引用的附件——只看得到会话不能证明附件齐全。
        「空白」要真的空白：用新系统账号、干净虚拟机或备用设备；Windows 上若用
        `RCLAW_HOME` 指到临时目录，先确认当前账号的 `%APPDATA%\rclaw` 下没有旧
        `rclaw.toml`——有的话启动会尝试把旧数据迁进来（整体移动失败时还会复制），演练就
        带了历史数据。
        空间重置会换密钥，旧备份只能用旧配对码恢复；Android 禁止应用系统备份，最后一台
        设备丢失时只有这些材料能恢复。
