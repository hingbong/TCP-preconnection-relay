# TCP-preconnection-relay

高性能TCP/UDP转发器，类似于realm，gost那种。采用零拷贝转发，性能基本无瓶颈。UDP性能良好。TCP连接采用了预链接方式，让线路鸡和落地鸡长期维持一个连接池，随时取用。故而消除了握手延时（长距离转发，如日本转发美国，效果尤为明显，理论上也可用于内网转发如Po0），客观数据上表现为http延时减少，同时也没有单纯连接复用的种种副作用。有完善的连接回收机制，避免了qos以及内存大量占用。

目前最新版支持出入站v4和v6，出站还支持使用域名。同时实现了单配置多转发，更加方便。

---

## 安装

一键安装（脚本指令已更新，请重新复制）：

```bash
curl -fsSL https://raw.githubusercontent.com/Xeloan/TCP-preconnection-relay/main/install.sh -o install.sh && bash install.sh
```

安装脚本会先安装 `git`，然后克隆仓库，优先读取仓库 `dist/` 里的同架构预编译二进制。当前预编译文件支持 `linux-amd64`、`linux-arm64`、`linux-armv7`，如果没有匹配架构，或者二进制无法运行，会自动回退到本地编译。

如果想强制源码编译：

```bash
TCP_POOL_PREBUILT=0 bash install.sh
```

如果要安装指定 tag/branch 的仓库版本：

```bash
TCP_POOL_VERSION=v1.6 bash install.sh
```

维护者可以通过 GitHub Actions 或本地脚本生成预编译文件并放入 `dist/`。也可以在当前机器手动构建本机架构二进制：

```bash
./build-release.sh
```

安装完成后输入管理命令：

```bash
relay
```

管理脚本支持创建/修改/删除转发、启动/停止实例、查看状态和日志、编辑配置、应用 TCP 调优、更新程序和卸载程序。创建或修改转发时，可以选择是否配置高级参数；不配置则使用程序默认值，不会把默认高级参数写进配置文件。执行 `relay update` 会同步更新主程序和 `relay` 管理脚本，并保留已有配置。

也可以直接使用子命令：

```bash
relay add
relay modify
relay delete
relay list
relay restart
relay logs
relay update
relay uninstall
```


## 常用命令说明

打开管理脚本：
```
relay
```

修改配置文件：
```
nano /etc/tcp_pool/relays.conf
```

应用配置并启动/重启全部转发：
```
tcp-pool-start
```

停止某个实例（把 HK 改成你自己的标签）：
```
systemctl stop tcp-pool@HK
```

禁用某个实例开机自启（把 HK 改成你自己的标签）：
```
systemctl disable tcp-pool@HK
```

查看某个实例日志（把 HK 改成你自己的标签），如果看到一坨Preconnect +1，说明成了：
```
journalctl -u tcp-pool@HK -f
```

## 更新日志
v1.6 意外发现mihomo貌似有些情况不会主动发EOF，导致连接数会膨胀，虽然有240s兜底但是不够优雅。于是优化了连接回收机制。

v1.5 优化半连接处理机制，避免了googleplay下载到94%卡住问题。感谢老虎哥发现bug。

v1.4 按照某位群友要求，新增了一键tcp调优。参数比较通用，适用于绝大部分机器。

v1.3 增加了出入站v4和v6支持，出站还支持使用域名。同时实现了单配置多转发，更加方便。旧版的友友们注意按照指南清空下配置。

## 指南
安装过程中会有保姆级指南。

## 效果示例

* 无预链接的转发（使用realm）：
<img width="2337" height="277" alt="image" src="https://github.com/user-attachments/assets/cba16059-ded2-43da-b571-0bcaff2ea70b" />

* 有预链接的转发:
<img width="2559" height="256" alt="image" src="https://github.com/user-attachments/assets/bc78e370-9072-4fb1-90fc-75d2a6304618" />

* 单线程测速：（需要调参）
<img width="2557" height="216" alt="image" src="https://github.com/user-attachments/assets/30c7c92e-c9d1-4f9d-80ee-9b41190a9d8f" />

* gomami最近被干了禁用了udp，实际上没问题，等好了我更新一下图片。

* 测试环境为上海移动，日本优化线路为gomami，美国西雅图落地为Bug Net（名字懒得改了），可见在转发路径高rtt情况下有明显的延时下降，同时单线程速率表现良好，和其它转发无异。

* 日本优化Gomami：https://www.gomami.io (贵，无aff）
* 美国西雅图落地：https://www.misaka.io (贵，无aff，商家也没开aff功能，国际互联非常优秀，但是日本到这家因为ddos，最近不太稳定）
* 美国西雅图落地：https://bug.pw?ref=Nifwr0tPxf （便宜点，日本过去延时稳定82ms，带宽现在比misaka足且稳定，所以有aff）
* 喜欢的话给我买包辣条
<img width="636" height="730" alt="image" src="https://github.com/user-attachments/assets/7a40db31-1e51-4e13-8aea-46f14f8ca6d1" />
