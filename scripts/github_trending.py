#!/usr/bin/env python3
"""GitHub 每日升星最快项目报告"""

import requests
import os
from datetime import datetime, timedelta

WORKSPACE = '/Users/william/q-harness'
REPORT_DIR = os.path.join(WORKSPACE, 'reports')

HEADERS = {
    'Accept': 'application/vnd.github.v3+json',
    'User-Agent': 'TrendingReportBot'
}


def search_repos(query, sort='stars', per_page=20):
    """Search GitHub repos"""
    url = "https://api.github.com/search/repositories"
    params = {'q': query, 'sort': sort, 'order': 'desc', 'per_page': per_page}
    try:
        r = requests.get(url, params=params, headers=HEADERS, timeout=30)
        if r.status_code == 200:
            return r.json().get('items', [])
    except Exception as e:
        print(f"[WARN] 搜索失败: {e}")
    return []


def get_trending_repos():
    """获取昨日活跃 & 近期热门项目"""
    yesterday = (datetime.now() - timedelta(days=1)).strftime('%Y-%m-%d')
    week_ago = (datetime.now() - timedelta(days=7)).strftime('%Y-%m-%d')

    results = {}

    # 1) 昨日有推送、总星数 > 500 的项目（活跃热门）
    q1 = f'pushed:>{yesterday} stars:>500'
    for item in search_repos(q1, per_page=30):
        results[item['full_name']] = item

    # 2) 近一周新建、星数 > 200 的项目（新晋黑马）
    q2 = f'created:>{week_ago} stars:>200'
    for item in search_repos(q2, per_page=20):
        results[item['full_name']] = item

    # 3) 昨日有推送、总星数 50~500 的上升期项目
    q3 = f'pushed:>{yesterday} stars:>50 stars:<500'
    for item in search_repos(q3, per_page=20):
        results[item['full_name']] = item

    return list(results.values())


def generate_report(repos):
    """生成 Markdown 报告"""
    today = datetime.now().strftime('%Y-%m-%d %H:%M')
    repos.sort(key=lambda x: x['stargazers_count'], reverse=True)
    top = repos[:15]

    lines = []
    lines.append(f"# 📊 GitHub 每日升星报告")
    lines.append(f"**生成时间**: {today}")
    lines.append(f"**数据来源**: GitHub Search API（昨日推送 + 近一周新建）")
    lines.append("")
    lines.append("---")

    for i, repo in enumerate(top, 1):
        desc = (repo.get('description') or '无描述')[:120]
        topics = ', '.join(repo.get('topics', [])[:4])
        lines.append(f"## {i}. ⭐ {repo['full_name']}")
        lines.append(f"- **链接**: {repo['html_url']}")
        lines.append(f"- **总星数**: {repo['stargazers_count']:,} ⭐ | Fork: {repo['forks_count']:,}")
        lines.append(f"- **语言**: {repo.get('language', 'N/A')}")
        lines.append(f"- **描述**: {desc}")
        if topics:
            lines.append(f"- **标签**: {topics}")
        lines.append(f"- **最近推送**: {repo['pushed_at'][:10]}")
        lines.append("")

    lines.append("---")
    lines.append("> 💡 说明：GitHub API 不提供精确的「昨日新增星数」，")
    lines.append("> 本报告通过「昨日活跃推送 + 星数规模 + 近期新建」综合筛选近似排名。")

    return '\n'.join(lines)


def main():
    print("[INFO] 正在搜索 GitHub 热门项目...")
    repos = get_trending_repos()
    print(f"[INFO] 共获取 {len(repos)} 个项目，生成报告...")

    report = generate_report(repos)

    # 保存报告
    os.makedirs(REPORT_DIR, exist_ok=True)
    filename = f"github_trending_{datetime.now().strftime('%Y%m%d')}.md"
    filepath = os.path.join(REPORT_DIR, filename)
    with open(filepath, 'w', encoding='utf-8') as f:
        f.write(report)

    # 打印到 stdout（cron 会记录到日志）
    print(report)
    print(f"\n[INFO] ✅ 报告已保存: {filepath}")


if __name__ == '__main__':
    main()
