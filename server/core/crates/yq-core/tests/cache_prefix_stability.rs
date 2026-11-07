//! KV-cache 前缀稳定性实测
//!
//! 硬性要求（阶段二）：
//!   · **零 LLM** —— 不调用任何模型
//!   · **零网络** —— 不发起任何请求
//!   · **零 DB**   —— 不连接数据库
//!   · 纯内存构造 10 组不同的（记忆 + 用户消息），只调 `StaticPrefix::build`
//!     与 `ContextBuilder::build`，比对前缀 SHA256
//!
//! 若 10 轮中出现任何一个不同的 SHA256，测试必须失败 —— **不许改断言**。

use std::sync::Arc;

use yq_core::message::Message;
use yq_core::persona::Persona;
use yq_core::prompt::{ContextBuilder, STATIC_RULES, SYSTEM_PROMPT, StaticPrefix};
use yq_core::tools::ToolRegistry;

/// 构造测试用 persona（常量内容，不读外部文件）。
fn test_persona() -> Persona {
    Persona {
        address: "大导演".to_string(),
        tone: "戏剧化".to_string(),
        language: "zh-CN".to_string(),
        forbidden_words: vec!["敷衍".to_string(), "随便".to_string()],
        preferences: vec!["中文".to_string(), "直接".to_string()],
        extra_rules: vec!["保持一致性".to_string(), "不臆造数据".to_string()],
    }
}

/// 构造测试用固定区前缀。
fn test_prefix() -> Arc<StaticPrefix> {
    let registry = ToolRegistry::new_default();
    let tools_prefix = registry.render_for_prefix();
    Arc::new(StaticPrefix::build(
        SYSTEM_PROMPT,
        &tools_prefix,
        &test_persona(),
        STATIC_RULES,
    ))
}

#[test]
fn cache_prefix_stable_over_10_rounds() {
    // 1) 固定区只构建一次
    let prefix = test_prefix();

    println!("================================================================");
    println!("KV-cache 前缀稳定性实测（10 轮）");
    println!("----------------------------------------------------------------");
    println!("固定区字节长度 : {}", prefix.len());
    println!("固定区 SHA256  : {}", prefix.sha256());
    println!("================================================================");

    let mut hashes: Vec<String> = Vec::new();

    for round in 1..=10i64 {
        // 2) 每轮构造不同的动态内容
        //    记忆数量随轮次递增 —— 验证"追加"不会影响前缀
        let memories: Vec<String> = (0..round)
            .map(|i| format!("mem[{}]: 第 {} 轮注入的记忆条目", i, round))
            .collect();

        // 历史对话（每轮不同）
        let history: Vec<Message> = vec![
            Message::user(format!("第 {} 轮的历史用户消息", round)),
            Message::assistant(format!("第 {} 轮的历史助手消息", round)),
        ];

        // 当前用户消息 —— 故意塞入随机数（验证随机数不进前缀）
        let randomish = round.wrapping_mul(7919).wrapping_mul(104729);
        let current = format!("第 {} 轮用户消息 rand={}", round, randomish);

        // 3) 组装完整上下文
        let mut builder = ContextBuilder::new(Arc::clone(&prefix));
        let built = builder
            .append_memories(&memories)
            .push_history(&history)
            .current_user(&current)
            .build();

        // 4) 取前缀 SHA256（直接来自 StaticPrefix，非反推）
        let h = built.prefix_sha256.clone();
        let dynamic_len = built.full_text.len().saturating_sub(prefix.len());

        println!(
            "轮 {:2} | 前缀 SHA256={} | 前缀字节={} | 动态区字节={}",
            round,
            h,
            prefix.len(),
            dynamic_len
        );

        hashes.push(h);
    }

    println!("================================================================");

    // 5) 断言 10 轮全等
    let first = &hashes[0];
    let all_same = hashes.iter().all(|h| h == first);
    if !all_same {
        for (idx, h) in hashes.iter().enumerate() {
            if h != first {
                eprintln!("❌ 第 {} 轮前缀 SHA256 漂移：{} != {}", idx + 1, h, first);
            }
        }
    }
    println!(
        "一致轮数 : {}/10",
        hashes.iter().filter(|h| *h == first).count()
    );
    println!("================================================================");

    assert!(
        all_same,
        "前缀 SHA256 出现漂移，违反 KV-cache 前缀固定硬性约束"
    );
}

/// 反证测试：改变 persona 必须改变前缀哈希。
///
/// 用来证明哈希确实在跟踪固定区内容，而不是恒定的常量。
#[test]
fn prefix_hash_changes_when_persona_changes() {
    let registry = ToolRegistry::new_default();
    let tools_prefix = registry.render_for_prefix();

    let mut persona_a = test_persona();
    let prefix_a = StaticPrefix::build(SYSTEM_PROMPT, &tools_prefix, &persona_a, STATIC_RULES);

    persona_a.tone = "严肃".to_string();
    let prefix_b = StaticPrefix::build(SYSTEM_PROMPT, &tools_prefix, &persona_a, STATIC_RULES);

    println!("persona A SHA256 : {}", prefix_a.sha256());
    println!("persona B SHA256 : {}", prefix_b.sha256());
    assert_ne!(
        prefix_a.sha256(),
        prefix_b.sha256(),
        "修改 persona 后前缀哈希未变化，说明哈希失真"
    );
}

/// 反证测试：动态区变化不影响前缀哈希。
#[test]
fn prefix_hash_unaffected_by_dynamic_content() {
    let prefix = test_prefix();

    let mut b1 = ContextBuilder::new(Arc::clone(&prefix));
    let out1 = b1
        .append_memories(&["只有一条记忆".to_string()])
        .current_user("消息一")
        .build();

    let mut b2 = ContextBuilder::new(Arc::clone(&prefix));
    let memories: Vec<String> = (0..50).map(|i| format!("记忆 {}", i)).collect();
    let out2 = b2
        .append_memories(&memories)
        .current_user("完全不同的用户消息 rand=999999")
        .build();

    println!("动态区 A 前缀 SHA256 : {}", out1.prefix_sha256);
    println!("动态区 B 前缀 SHA256 : {}", out2.prefix_sha256);
    assert_eq!(
        out1.prefix_sha256, out2.prefix_sha256,
        "动态区变化竟然影响了前缀哈希"
    );
    // 同时确认动态区内容确实不同（full_text 长度不同）
    assert_ne!(out1.full_text, out2.full_text);
}

/// 反证测试：五工具顺序固定且可复现。
#[test]
fn tool_registry_order_is_fixed() {
    let r1 = ToolRegistry::new_default();
    let r2 = ToolRegistry::new_default();

    let names1: Vec<&str> = r1.definitions().iter().map(|d| d.name).collect();
    let names2: Vec<&str> = r2.definitions().iter().map(|d| d.name).collect();

    println!("工具顺序: {:?}", names1);
    assert_eq!(names1, names2, "两次构建的工具顺序不一致");
    assert_eq!(
        names1,
        vec!["read", "write", "edit", "bash", "delegate"],
        "工具顺序不符合固定约定"
    );

    // 前缀渲染也必须字节级一致
    assert_eq!(
        r1.render_for_prefix(),
        r2.render_for_prefix(),
        "工具定义前缀渲染不稳定"
    );
}
