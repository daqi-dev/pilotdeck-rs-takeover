# steer attachments fixed oracle and formal gate

`oracle.mjs`调用固定cc7c4e6真实InProcessGateway.steerTurn与真实composer，只在active/router结果边界捕获。Node22.22.0，隔离HOME/PILOT_HOME、实际临时文件+symlink，源码/dist转译逐字节核对。a/b两轮26格hash b4021afdb238bd1f691ab5416f33700d5b5e54eda7a37e8477bb608ae695a32b，原始`_tmp/20261113-steer-attachments-oracle-{a,b}`。这不是正式server验证。

正式`turn-steer-attachments-native.json`由E0在cb8固定source/harness snapshot执行，不能在本机18789既有服务旁绕过guard。沿既有WS事件answer模式，真实活动bash开始时steer真实注册附件；只给初始bash一次权限allow，后续模型必须收到附文/注册path note，再调用真实read_file读取同一外部隔离tmp路径。完成后校验durable附件steer。`verify-formal.mjs artifact.json`是额外严格门，不修改run/evidence，不用SAME代替原始正向断言。

负控应在私有新场景副本只改附件content或去掉metadata.channelKey，然后运行实际链，要求正式模型marker/读文件授权断言失败；不能把原场景超时或环境失败当负控有效。当前formal/负控均未运行，不伪称通过。Rust private2tests和旧两块原门顺序测试也尚未集中编译/执行。
