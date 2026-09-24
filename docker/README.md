# docker/ — SWE-bench 评测镜像

本目录是 SWE-bench 评测基础设施的容器侧配套,与 `crates/astrcode-eval` 和
`crates/astrcode-cli` 的评测路径配合使用,不参与日常构建与发布流程。

## 文件一览

| 文件 | 用途 |
|---|---|
| `swebench-solver.Dockerfile` | solver 镜像:以 nightly 工具链构建 release 版 `astrcode-cli --features dev-mode`,装入 bookworm-slim 执行评测用例 |
| `swebench-egress.Dockerfile` | 出口网关镜像:tinyproxy + 出口过滤 + provider 网关,把 solver 的网络出口限制到仅可访问模型 provider |
| `swebench-egress-tinyproxy.conf` / `swebench-egress-filter` | egress 镜像内的代理配置与过滤规则 |
| `swebench-provider-gateway.py` | provider 网关服务(solver 经 `http://astrcode-swebench-egress:8080` 访问) |
| `swebench-egress-entrypoint` | egress 容器入口脚本 |
| `swebench-control-relay.py` | 控制面 relay,装入 egress 容器 `/usr/local/bin/swebench-control-relay.py` |

## 构建

构建上下文必须是仓库根目录(Dockerfile 内 COPY 路径以 `docker/` 开头):

```sh
docker build -f docker/swebench-solver.Dockerfile -t astrcode-swebench-solver .
docker build -f docker/swebench-egress.Dockerfile -t astrcode-swebench-egress .
```

## 与代码的关联

修改以下位置时需要同步本目录:

- `crates/astrcode-cli/src/main.rs`:`astrcode-swebench-control` /
  `astrcode-swebench-egress` 等网络与镜像名是 CLI 参数的默认值。
- `crates/astrcode-eval/src/swebench_instance.rs`:provider 网关地址
  (`http://astrcode-swebench-egress:8080`)与 control relay 的容器内路径
  (`/usr/local/bin/swebench-control-relay.py`)。
