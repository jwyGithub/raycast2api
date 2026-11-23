# Raycast 客户端 TLS 指纹实测(备查档)

> 日期:2026-11-22。目标:确认本地 Raycast Windows 客户端发起 `backend.raycast.com` 对话请求时的 TLS 指纹。
> 方法:定位 Raycast 2.13.1.0 (MSIX) 内嵌 Node.exe,拷出后以它直接请求 `https://tls.peet.ws/api/all` 回显 ClientHello,提取 JA3/JA4/Akamai 指纹。
> 结论可信度:栈判定与指纹均为实测;唯一假设是 Raycast 的对话请求走该 Node 进程默认 TLS 栈(由 `index.mjs` 的选项管道全默认值佐证,见 §3)。

---

## 1. 栈判定(实测)

Raycast 2.13.1.0 Windows(安装位置 `C:\Program Files\WindowsApps\Raycast.Raycast_2.13.1.0_x64__qypenmj9wpt2a`)是 **.NET 10 WPF 壳 + 内嵌 Node.js** 的结构,不是 Electron:

- 壳:`Raycast.exe`(net10.0,自带 .NET 10.0.13 运行时,Schannel TLS 栈,但**不承载 AI 请求**)
- AI 后端请求:由 `backend/index.mjs` 在内嵌 `backend/node.exe` 进程内发出

内嵌 Node 版本(实测 `process.versions`):

| 组件 | 版本 |
| --- | --- |
| Node | 26.13.1 |
| OpenSSL | 3.5.9 |
| undici | 8.12.4 |
| nghttp2 | 1.70.0 |

即对话请求的 TLS 栈是 **Node 内置 OpenSSL 3.5.9 + undici fetch**,不是 Chromium/BoringSSL,也不是 Schannel。

## 2. ClientHello 指纹(2026-11-22/23 实测快照)

客户端有 **两条 TLS 出口**(同一 Node 二进制、同一 OpenSSL 3.5.9,差异仅在扩展集与 HTTP 版本):

### 2.1 undici fetch 管道(chat 对话请求,h2)

| 项 | 值 |
| --- | --- |
| **JA3 hash** | `d67b094811e5145139d7cea5f014309f` |
| **JA3** | `771,4866-4867-4865-49199-49195-49200-49196-158-49191-103-49192-107-163-159-52393-52392-52394-49325-49311-49245-49249-49239-49235-162-49324-49310-49244-49248-49238-49234-49188-106-49187-64-49162-49172-57-56-49161-49171-51-50-157-49309-49233-156-49308-49232-61-60-53-47,65281-0-11-10-35-16-22-23-13-43-45-51,4588-29-23-30-24-25-256-257,0-1-2` |
| **JA4** | `t13d5212h1_b262b3658495_8e6e362c5eac` |
| **Akamai h2 指纹** | `2:0;4:262144\|458753\|0\|a,m,p,s`(hash `75560cd255283a449819f60ea47983aa`) |
| ALPN | `http/1.1, h2`(对话请求实际走 h2) |
| TLS 版本 | TLS 1.3(带 1.2 兼容套件) |
| 密码套件 | 52 个,顺序:TLS13 三件套(`AES_256_GCM`/`CHACHA20`/`AES_128_GCM`)排头,后接 ECDHE/DHE/RSA 全家 —— OpenSSL 3.x 默认序 |
| 扩展顺序 | `renegotiation_info(65281), server_name(0), ec_point_formats(11), supported_groups(10), session_ticket(35), ALPN(16), encrypt_then_mac(22), extended_master_secret(23), signature_algorithms(13), supported_versions(43), psk_key_exchange_modes(45), key_share(51)` |
| supported_groups | `X25519MLKEM768(4588), X25519(29), P-256(23), X448(30), P-384(24), P-521(25), ffdhe2048(256), ffdhe3072(257)` |

特征速记:OpenSSL 3.x 默认套件顺序、带后量子 `X25519MLKEM768`、无 padded extensions —— 一眼可辨"Node/undici 默认栈",与 Chrome(BoringSSL)、.NET HttpClient(Schannel)指纹完全不同。

### 2.2 got 管道(其余全部 API,HTTP/1.1)(2026-11-23 实测)

Node `https` 模块与 undici fetch 共用 OpenSSL,但 **ClientHello 不带 ALPN 扩展**(HTTP/1.1 无应用层协议协商需求),其余参数与 §2.1 完全一致:

| 项 | 值 |
| --- | --- |
| **JA3 hash** | `944d1e1858cd278718f8a46b65d3212f` |
| **JA3** | 与 §2.1 相同,但扩展段少了 ALPN:`771,4866-4867-4865-…同上…,65281-0-11-10-35-22-23-13-43-45-51,4588-29-23-30-24-25-256-257,0-1-2` |
| **JA4** | `t13d5211_b262b3658495_8e6e362c5eac`(末段同 §2.1;`11` = 11 个扩展、无 `h1/h2` ALPN 标记) |
| HTTP 版本 | HTTP/1.1(got 不启用 h2) |

即:上游若做指纹校验,看到的不是单一指纹,而是按端点分叉的两套 —— 对话请求带 ALPN/走 h2,其余 API 无 ALPN/走 HTTP/1.1。

## 3. 无自定义 TLS 覆盖的佐证

`backend/index.mjs` 中 grep 到的 `ciphers`/`minVersion`/`alpnProtocols`/`Agent(` 全部属于 HTTP 客户端选项解析管道(每项默认 `undefined`),未发现任何对 cipher 列表、扩展顺序、ALPN 的硬编码覆盖。即 Raycast 发出的就是该 Node 二进制的默认指纹。

## 4. 对本网关的意义

- 本网关(reqwest + rustls)的出站指纹是 rustls 默认:无 `X25519MLKEM768`,套件集与顺序不同,JA3 与真客户端不一致。
- **目前没有证据表明 Raycast 后端做 TLS 指纹校验**(已知的风控是 User-Agent 版本判定,见 `model-degradation.md`;rustls 栈一直正常工作)。因此 TLS 层暂不对齐,本文件仅备查。
- 触发条件:若上游开始 403/握手期断连且换 UA 无效,先对 JA3/JA4 排查,再考虑对齐。

### 4.1 HTTP 头层对齐(2026-11-23 已完成)

TLS 之外的头层已按本次逆向结论对齐(`src/raycast/client.rs` 的 `Transport`):

- chat(`ai/chat_completions`,undici fetch 实测自动头):`accept-encoding: br, gzip, deflate, zstd`、`accept-language: *`、`sec-fetch-mode: cors`,UA 用 `x-Windows Version` 变体。
- 其余 API(/me、credits、models、`ai/files` 注册,内部走 got):`accept-encoding: gzip, deflate, br, zstd`,UA 用 `x-Windows-amd64` 变体,无 sec-fetch-mode / Experimental。
- presigned PUT(got 裸实例):仅服务端下发头 + got 自动 `accept-encoding`,无默认 UA。
- reqwest 开启 gzip/brotli/deflate/zstd 特性仅为响应自动解压;tower-http 解压层在请求已带 `accept-encoding` 时不覆盖(`Entry::Vacant` 语义),显式头原样发出。

### 4.2 各上游端点的管道与 TLS 指纹对照(2026-11-23 逆向定性)

`src/config.rs` 中全部上游端点,按真实客户端发请求的管道归类:

| 端点(config.rs 常量) | 客户端管道 | UA 变体 | TLS 指纹 |
| --- | --- | --- | --- |
| `RAYCAST_API_URL` `/ai/chat_completions`(含 `/resume`) | undici fetch | `x-Windows Version` | §2.1(JA3 `d67b0948…`,h2) |
| `RAYCAST_ME_URL` `/me`、`/me/trial_status` | got(`Zy`) | `x-Windows-amd64` | §2.2(JA3 `944d1e18…`,h1.1) |
| `RAYCAST_AI_CREDITS_URL` `/ai/credits`、`_DETAILS_` `/details` | got(`Zy`) | `x-Windows-amd64` | §2.2 |
| `RAYCAST_MODELS_URL` `/ai/models` | got(`Zy`) | `x-Windows-amd64` | §2.2 |
| `RAYCAST_FILES_URL` `/ai/files` 注册 POST | got(`Zy`,实测带签名三件套+DeviceId,无 Experimental) | `x-Windows-amd64` | §2.2 |
| presigned PUT(direct_upload 目标,非 backend 域) | got 裸实例(`tb`,无默认 UA/无 UA 头) | 无 | §2.2 |
| `OAUTH_TOKEN_URL` `/oauth/token`(`$y` 实例,login/refresh/revoke 共用) | got(`$y`,prefixUrl=backend,UA=Wy) | `x-Windows-amd64` | §2.2 |

网关现状:reqwest+rustls 对所有端点同一 rustls 默认指纹,既非 §2.1 也非 §2.2;若未来要对齐,优先级最高的是 chat(h2、带 ALPN),其次是 got 系(HTTP/1.1、无 ALPN 扩展)。

## 5. 若需对齐(方案备忘)

| 方案 | 内容 | 评价 |
| --- | --- | --- |
| wreq + 自定义 ClientHello | wreq(原 rquest,reqwest fork)支持逐项配置 cipher 顺序/扩展/ALPN/key share;按 §2 参数手工构造 Node 指纹。**注意 wreq 无 Node 预设**,`impersonate` 档位全是浏览器(Chrome/Firefox/Safari/Edge/OkHttp) | 可行,推荐。代价:Raycast 每次升级内嵌 Node 指纹即变(2.13.1 已从旧版跳到 Node 26.13),需持续重抓更新 |
| rustls 调参 | rustls 不暴露扩展顺序控制,cipher 顺序受限,JA4 第二段对不上 | 基本不可行 |
| 自建 BoringSSL transport | 等价于重写 wreq | 不值得 |

## 6. 快照失效提醒

指纹与 Raycast 内嵌 Node 版本绑定。复查方法:

1. `Get-AppxPackage -Name '*Raycast*'` 取版本与安装路径;
2. 拷出 `...\Raycast\backend\node.exe`(WindowsApps ACL 直接执行被拒,先 Copy-Item);
3. `<拷出的 node.exe> -p "JSON.stringify(process.versions)"` 记录版本;
4. 用它请求 `https://tls.peet.ws/api/all`,比对 §2:
   - `fetch('https://tls.peet.ws/api/all')` → §2.1(undici fetch,h2);
   - `require('https').request({host:'tls.peet.ws',path:'/api/all'})` → §2.2(got 底层,HTTP/1.1,无 ALPN 扩展)。

若 Node 主版本变化,§2 全表作废,需重测。
