# xtervo 开发日程 / 本地提醒

这是**本地提醒器**，不是 agent 自动调度器。

它能做：

```text
到点写 .logs/dev-schedule.log
调用 notify-send 发桌面通知
记录已触发，避免重复
```

它不能做：

```text
唤醒 LLM session
自动开始写代码
自动提交/合并/打包
```

原因：当前环境没有可用的 systemd user bus，`crontab` 没有权限，agent 也没有
timer skill。不要在文档里把“提醒”写成“自动开发”。

## 使用

```bash
# 只检查一次
python3 scripts/dev_schedule.py

# 常驻，每 20 秒检查
python3 scripts/dev_schedule.py --daemon
```

日志：

```text
.logs/dev-schedule.log
```

状态（已触发 id）：

```text
.logs/dev-schedule-state.json
```

## 日程

```text
周五 18:00  MCP 配置化开工
周五 20:30  默认 socket/token + Settings UI
周五 23:00  MCP e2e / checkpoint
周六 09:30  周六开工；先收尾 MCP
周六 13:30  unclean shutdown + 恢复提示
周六 19:00  session fsync + last-known-good
周六 23:30  15m soak + commit
周日 09:30  crash marker + safe mode
周日 13:30  支持包 + 恢复矩阵
周日 19:00  CI + 30m + 真机（如可用）
周日 23:00  全绿才打 RC3
```

## 说明

真正的“到点自动让 agent 干活”需要外部调度器调用一个 agent CLI/daemon；
本仓库目前没有这条链路，也不应该在没有隔离和审批的情况下装。  
这份日程只负责提醒和留痕，开发仍由当前 session 执行。
