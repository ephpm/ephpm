# Popularity & Adoption Analysis

*Data collected March 2026*

---

## GitHub Stars

| Project | Stars | Forks | Open Issues | Created |
|---|---|---|---|---|
| **Caddy** | 70,826 | 4,675 | 241 | Jan 2015 |
| **Swoole** | 18,861 | 3,160 | 6 | Jul 2012 |
| **FrankenPHP** | 10,905 | 439 | 185 | Mar 2022 |
| **RoadRunner** | 8,418 | 420 | 82 | Dec 2017 |
| **Hyperf** (Swoole framework) | 6,793 | 1,291 | — | Jun 2019 |
| **Laravel Octane** | 3,998 | 334 | 20 | Mar 2021 |
| **OpenSwoole** (fork) | 846 | 55 | 42 | — |

Caddy dominates by an order of magnitude, but it's a general-purpose web server competing with nginx/Apache — different category. Among PHP-specific servers, Swoole leads in raw stars (longest-running, huge Chinese developer base), FrankenPHP is the fastest-growing (10.9k stars in ~3 years vs RoadRunner's 8.4k in ~8 years).

---

## Docker Hub Pulls

| Image | Pulls |
|---|---|
| **library/caddy** (official) | 678,262,745 |
| **dunglas/frankenphp** | 5,107,021 |
| **phpswoole/swoole** | 1,656,424 |
| **spiralscout/roadrunner** | 1,033,241 |
| **openswoole/swoole** | 199,596 |

Caddy's official image has 678M+ pulls — it's in a completely different league as a general web server. Among PHP servers, FrankenPHP leads with 5.1M pulls (boosted by Laravel Cloud and Symfony adoption). RoadRunner and Swoole are in the 1-1.7M range.

Note: Swoole's Docker numbers undercount its actual usage since many users install via PECL directly into custom images rather than using the pre-built image.

---

## Packagist Downloads (PHP packages)

| Package | Total Installs | Monthly | Daily |
|---|---|---|---|
| **laravel/octane** | 20,859,916 | 1,298,655 | 18,589 |
| **spiral/roadrunner-worker** | 10,280,774 | 536,022 | 4,579 |
| **spiral/roadrunner-http** | 8,970,693 | — | — |
| **openswoole/core** | 1,043,112 | 75,966 | 3,181 |
| **hyperf/framework** | 2,756,707 | — | — |

Laravel Octane is the dominant PHP package at ~1.3M monthly downloads. This is the primary way most PHP developers interact with these servers. RoadRunner's worker package gets ~536k monthly — a significant portion of which is driven by Octane installations (Octane depends on it when using RoadRunner driver).

FrankenPHP has no Packagist package — it's a Go binary. This means its PHP-side adoption is invisible in Packagist stats, making it harder to track but also demonstrating its zero-dependency philosophy.

---

## Community & Ecosystem

### Reddit

Reddit removed public subscriber counts in late 2025, replacing them with "weekly visitors" and "contributions" metrics. Approximate data from before that change and current activity:

- **r/PHP** — ~191k members (before change). Active discussions comparing FrankenPHP/RoadRunner/Swoole are frequent.
- **r/laravel** — Large community. Octane and FrankenPHP are regularly discussed topics.
- **r/CaddyServer** — Small dedicated community. Caddy discussions also happen in r/selfhosted and r/homelab.
- No dedicated subreddits exist for FrankenPHP, RoadRunner, or Swoole individually.

### Framework Integration

| Framework | FrankenPHP | RoadRunner | Swoole |
|---|---|---|---|
| **Laravel** (via Octane) | First-class | First-class | First-class |
| **Symfony** (via Runtime) | First-class | Community bundle | Supported |
| **API Platform** | Default server | — | — |
| **Spiral** | — | Native (creator) | — |
| **Yii** | — | Official runner | — |
| **Hyperf** | — | — | Native (built on it) |

---

## Business Adoption

### FrankenPHP
- **PHP Foundation** — officially supported since May 2025, code moved to PHP GitHub org
- **Laravel Cloud** — default server for Laravel's hosting platform
- **Upsun** (Platform.sh) — supported deployment target
- **Clever Cloud** — official FrankenPHP support
- **Les Tilleuls** — Kevin Dunglas's company, API Platform creator
- **API Platform** — default recommended server

### RoadRunner
- **Spiral Scout** — creator and primary maintainer (software consultancy)
- **Temporal Technologies** — RoadRunner is the worker runtime for Temporal's PHP SDK
- Widely adopted in enterprise Laravel and Symfony deployments
- Most battle-tested option (production since 2018)

### Swoole
- **119 companies tracked** by TheirStack.com
- **Zendesk** — notable user
- **Grupo Boticário** — Brazilian personal care giant (47k employees)
- **PicPay** — Brazilian fintech
- **Tencent** — Swoole/TARS integration used in QQ Browser, Tencent App Store, handling ~10 billion requests/day
- Massive adoption in Chinese tech ecosystem (Swoole originated in China)
- Hyperf framework used privately in series-B/C internet companies before open-sourcing

### Caddy
- **137,376 companies** tracked by Enlyft
- **0.53% market share** in web servers (vs Apache 42.76%, nginx 38.6%)
- **83% small companies** (<50 employees), 6% large (>1,000)
- **33% US**, **19% Germany**
- Acquired by **Apilayer** (2020), commercial support via **Ardan Labs**
- Used by **Platform.sh**, **AppSumo**, **iAdvize**

### Laravel Octane
- ~1.3M monthly Packagist downloads
- 30% uptick in adoption reported in 2025 Laravel Developer Survey
- Default for Laravel Cloud deployments
- Not a server itself — adoption tracks to FrankenPHP/RoadRunner/Swoole

---

## Estimated Monthly Active Users / Deployments

These are rough estimates based on download velocity, Docker pulls, Packagist daily downloads, and business adoption signals:

| Project | Estimated Monthly Active Deployments | Estimated Monthly Active Developers | Confidence |
|---|---|---|---|
| **Caddy** | 100,000 – 200,000+ | 50,000+ | Medium (general web server, hard to isolate PHP use) |
| **Swoole** | 15,000 – 30,000 | 8,000 – 15,000 | Low (Chinese ecosystem largely invisible to Western metrics) |
| **RoadRunner** | 8,000 – 15,000 | 4,000 – 8,000 | Medium (Packagist daily downloads + Docker pulls) |
| **FrankenPHP** | 5,000 – 12,000 | 3,000 – 7,000 | Medium (Docker pulls growing fast, but newer project) |
| **Laravel Octane** | 15,000 – 25,000 | 10,000 – 20,000 | Medium-High (Packagist data is reliable; ~18.5k daily downloads) |

### Methodology notes

- **Packagist daily downloads** are the best signal for PHP-side adoption. Octane's 18.5k daily downloads include CI/CD pipelines and dev environments, so actual production deployments are a fraction (typically 10-30% of daily downloads).
- **Docker pulls** are cumulative and include CI, so monthly active is much smaller than total. FrankenPHP's 5.1M total over ~3 years suggests ~140k pulls/month, but many are automated.
- **Swoole is undercounted** in Western metrics. Its Chinese user base (Tencent ecosystem, Hyperf framework, Chinese startups) is enormous but doesn't show up in English-language tracking tools. The 18.8k GitHub stars and Hyperf's 6.8k stars suggest a large active community.
- **Caddy is overcounted** for PHP relevance — most Caddy users are not running PHP. Its 678M Docker pulls serve the general web server market.
- **Laravel Octane** overlaps with FrankenPHP/RoadRunner/Swoole numbers since it's an adapter, not a server. Its downloads partially double-count the underlying server adoption.

---

## Growth Trajectory

| Project | Stars/year (approx) | Momentum |
|---|---|---|
| **FrankenPHP** | ~2,700/yr (10.9k in 4 years) | **Accelerating** — PHP Foundation backing, Laravel Cloud default, fastest-growing in category |
| **RoadRunner** | ~1,050/yr (8.4k in 8 years) | **Steady** — mature, reliable, but not capturing new mindshare as aggressively |
| **Swoole** | ~1,350/yr (18.8k in 14 years) | **Plateauing** — OpenSwoole fork fragmented community, Western adoption stalled |
| **Caddy** | ~6,440/yr (70.8k in 11 years) | **Strong** — established, growing steadily in the general web server space |
| **Laravel Octane** | ~800/yr (4k in 5 years) | **Steady** — tied to Laravel's growth, boosted by Laravel Cloud |

### Key takeaway

FrankenPHP has the strongest growth momentum in the PHP server space. Its backing by the PHP Foundation, adoption as Laravel Cloud's default, and zero-dependency PHP integration make it the project to watch. RoadRunner is the incumbent with the most mature ecosystem. Swoole has the largest raw user base (especially in China) but growth has stalled in Western markets and the OpenSwoole fork fragmented the community.

For ePHPm, the competitive window is now — before FrankenPHP expands into the feature gaps (DB pooling, clustering, observability) that currently define ePHPm's value proposition.
