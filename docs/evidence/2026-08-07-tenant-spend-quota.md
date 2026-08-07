# 租户日消费上限，以及两条「不出声的错」怎么被逼出声

日期：2026-08-07
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 起因

并发配额（V27）挡住的是「一个租户吃满平台所有 Worker 槽位」。它对**花钱**毫无约束：
一个始终只开两个并发 Run 的租户，可以一个接一个地无限花下去。复审要求的
「租户并发/Token/费用配额」里，只有并发那一项做完了。

## 不新建用量账本

消耗**已经**持久化在 `run_events` 里，是 Worker 写的 `model.usage` 事件载荷，
带 `input_tokens`、`output_tokens`、`cost_micros`，与其他所有状态走同一条事件流对账。

再建一张表就是**第二个真相**，两边会在第一次事件重放或一次写入丢失时分叉，
而且分叉之后没有任何办法判定哪一边是对的。所以准入直接在事件上做窗口聚合：

```sql
select coalesce(sum((payload->>'cost_micros')::bigint), 0),
       coalesce(sum((payload->>'input_tokens')::bigint
                  + (payload->>'output_tokens')::bigint), 0)
  from run_events
 where tenant_id = ? and type = 'model.usage'
   and occurred_at >= now() - interval '24 hours'
```

代价是每次准入要聚合一天的事件，用 `(tenant_id, occurred_at) where type = 'model.usage'`
的部分索引把它限制在真正相关的行上。

检查放在**已经持有配额行锁之后**，与并发检查同一个事务、同一把锁，不额外引入一次串行化。

## 两个默认值，两次故障注入

这一片里有两个「写错了也照样跑，只是安静地错」的地方。各注入一次，证明测试真的会响。

### 注入一：把滚动窗口去掉，让上限变成累计总额

只删 `and occurred_at >= now() - interval '24 hours'`，其余不动：

```
Tests run: 16, Failures: 1
JdbcRunRepositoryIntegrationTest.spendOutsideTheWindowDoesNotCountAgainstTheCeiling:693
```

**只有那一条失败**，正是钉这件事的那条。没有窗口的上限永不遗忘，一个租户会因为
两天前一个昂贵的下午被永久锁死——而在没有这条测试时，它和正确实现在当天的行为完全一致，
要等到第二天才开始错。

### 注入二：把未设置的上限读成 0 而不是无限

只加一行 `if (cost == null) { cost = 0L; }`：

```
Tests run: 16, Failures: 1, Errors: 10
  anUnsetSpendCeilingMeansUnlimitedRatherThanNothingAllowed:707
    » TenantQuotaExceeded tenant has reached its daily spend ceiling of 0 micros
  ...另外 10 条同样原因
```

16 条里炸了 11 条。这个爆炸半径本身就是证据：**空值读成零 = 上线当天挡住所有租户**。
真正钉住它的是那条专门的用例，其余 10 条是连带——但连带的规模说明了这个错有多贵。

两次注入后均已还原，还原后全量绿。

## 顺带修的工具缺陷

故障注入第一次想跑单个测试类时又撞上 `mvn -Dtest=...` 的
`ExceptionInInitializerError`——集成测试要 `run-java-tests` 注入的数据库环境变量。
**这个坑本会话已经踩过两次**，说明它不是记性问题而是工具缺口。
给 `run-java-tests` 加了参数透传，并把踩坑原因写在脚本注释里：

```
deploy/native/run-java-tests -Dtest=JdbcRunRepositoryIntegrationTest
```

单类跑完 1.5–2.1 秒，全量十几分钟。故障注入从「要不要做」变成了没有理由不做。

## 一处刻意的分歧：Retry-After 300 秒

并发配额给 30 秒，消费上限给 300 秒。因为**消费上限随时间窗滚动而释放，
不随别的 Run 结束而释放**，30 秒后重试几乎必然再次被拒。给一个已知无用的重试提示，
比不给更糟。

## 检查结果

```
Java（run-java-tests）  154 通过 / 0 失败 / 1 跳过
```

## 明确不声称

- **上限在 Run 开始前检查，不在运行中检查。** 单个 Run 仍可能冲过上限。
  它约束的是「租户能开始多少工作」，不是「单个 Run 消耗多少」——后者由 Run 级预算约束。
  这一句同时写在代码注释里，不只写在这里。
- **Token 上限的列和判定都有，但没有专门的用例。** 费用那条钉住了共用的窗口与空值语义，
  Token 走的是同一段代码的另一个分支；在补上用例之前，不声称 Token 上限已被验证。
- **没有 API 可以设置这些上限。** 只能直接写库。租户自助配置是另一片工作。
- 未做 1000 Run 级压测，因此不声称该规模下的聚合开销可接受。
- 加权公平调度仍未做：配额只能拒绝，不能在多个租户之间排序。
