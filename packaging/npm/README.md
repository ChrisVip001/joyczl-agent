# joyczl-agent（npm 垫片）

Joy 本体是 Rust 二进制；这个包只做两件事：**安装时**按平台从 GitHub Releases
下载对应的 `joy`，**运行时**把参数原样转发给它。

```sh
npm install -g joyczl-agent
joy --version
```

- 支持平台：macOS（arm64/x64）、Linux（x64/arm64）。Windows 用 WSL2 跑 Linux 版。
- 二进制名约定：`joy-v<版本>-<target>.tar.gz`（由 `.github/workflows/release.yml`
  产出，两边必须一致）。
- 下载失败/手动安装/源码安装的指引见 `install.js` 的 fail 输出。

发布这个包：见 `packaging/RELEASING.md`（需要 npm 账号与 `NPM_TOKEN`，
实际 `npm publish` 由人执行）。
