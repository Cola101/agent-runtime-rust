# 加权公平派发，以及一次「测试通过但什么都没测」的现场

日期：2026-08-07
主机：MacBookPro18,3 / Apple M1 Pro / 16GB / macOS Darwin 25.5.0

## 起因

Outbox 派发是**全局严格 FIFO**：

```sql
order by created_at, id
```

一个租户一次塞进 10,000 条，之后每一批都被它填满，其他租户不论工作多小都排在后面，
一直等到这个积压排空。

**配额解决不了这件事。** 配额拒绝准入，它对「已经准入的那些的顺序」一个字都没说。
这两件事经常被混为一谈，但它们连作用的时刻都不同。

## 做法：位置除以权重

```sql
row_number() over (partition by tenant_id order by created_at, id) as position,
coalesce(q.dispatch_weight, 1) as weight
...
order by position::numeric / weight, created_at, id
```

权重 4 的租户在它第 4 条消息上到达虚拟位置 1，权重 1 的租户在它第 1 条上到达同一位置，
于是批次按 4:1 分配。权重相等就退化成朴素轮转。

**跨租户重排，租户内绝不重排**：租户内仍是最旧优先，所以一条有序的事件流不会被打乱。

权重放在 `tenant_run_quotas` 上，不另立表——那一行本来就是租户级准入旋钮，
另立一张表只会多一个「这个租户到底受哪些限制」的查找位置。默认 1，
所以现有租户行为不变。

## 四条测试，两条是行为两条是护栏

RED 阶段（FIFO 实现下）：

```
aTenantWithABacklogDoesNotDelayAnotherTenantsFirstMessage   失败
dispatchWeightDividesTheBatchInProportion                   失败
aTenantWithNoQuotaRowIsStillDispatched                      通过
messagesFromOneTenantKeepTheirOrder                         通过
```

后两条在 FIFO 下**本来就成立**，它们不是待实现的行为，是「改完之后不许坏掉」的护栏。

## 故障注入，以及两次注入互相掩盖

先同时注入两处（把租户内排序反向、把权重兜底去掉），失败了 2 条，
但**不是我预期的那 2 条**。原因是第二处注入让 weight 为 null，虚拟位置整列变 null，
排序退回 `created_at`，**恰好把第一处注入抵消掉了**。

一次只注一处，这才是能读的结果：

| 注入 | 被谁抓住 |
| --- | --- |
| 去掉 `coalesce(weight, 1)` | `aTenantWithNoQuotaRowIsStillDispatched` |
| 租户内排序反向 | `messagesFromOneTenantKeepTheirOrder` |

第一条能抓住，是因为我在写完之后把它加强了：原版只有一个无配额行的租户，
**没有竞争者**，权重为 null 排到最后也照样被取走，那条测试当时什么都没测。
加了一个有配额行、有积压的租户之后它才开始有意义——而这一点是注入逼出来的，不是看出来的。

## 一个既有缺陷：返回顺序从来没有被保证过

单独注入「租户内排序反向」时，`messagesFromOneTenantKeepTheirOrder` **仍然通过**。
排查后是这个：

`UPDATE ... RETURNING` **没有定义行顺序**。CTE 里的 `order by` 只决定取哪些行，
不决定它们以什么顺序返回。而 `OutboxPublisher` 是按 list 顺序逐条发布的
（`OutboxPublisher:32`），**所以消息进 NATS 的顺序一直取决于执行计划**。

这不是本次改动引入的，FIFO 版本同样如此，只是它「碰巧」按最旧优先返回，
而没有任何东西要求它必须如此。改成：

```sql
claimed as (update ... returning ...)
select * from claimed order by created_at, id
```

同时把那条顺序测试改成**只取 3 条而队列里有 5 条**——批次大到能装下全部时，
无论排序怎么反，取到的都是同一个集合，断言照样成立，测的是空气。

## 检查结果

```
Java（run-java-tests）  158 通过 / 0 失败 / 1 跳过
```

## 明确不声称

- **排名对每次轮询的全部可领取行计算**，代价随积压增长而不是随批次大小增长。
  加了 `(tenant_id, created_at, id) where published_at is null` 的部分索引把它限制在未发布行上。
  **已测量**：1000 条积压下单次领取最慢 14 毫秒（见
  [1000 Run 压测](2026-08-07-thousand-run-load.md)），未构成问题，
  因此不做按租户 lateral 取头部的优化。**只在 1000 这一个量级测过**，不向上外推。
- **没有 API 可以设置权重。** 只能直接写库，与消费上限同样的缺口。
- 公平性只作用在 Outbox 派发这一段。Worker 侧的槽位分配不在本次范围内。
- 权重没有做「未使用配额可被他人借用」的工作量守恒（work-conserving）语义之外的验证：
  单租户在场时它当然拿到整批，但没有专门用例钉住这一点。
