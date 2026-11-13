//! 文件视图相关模型：排序比较器、视图模式与可见区请求。

use crate::error::Result;

/// 轻量文件条目：先提交轻量字段，图标与缩略图异步补齐。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<u64>,
    pub modified: Option<u64>,
    pub created: Option<u64>,
    /// 稳定身份（卷 + 文件 ID，路径回退）。
    pub identity_key: Option<u64>,
}

/// 与 Windows 资源管理器一致的本地语言、大小写及数字排序。
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    #[cfg(windows)]
    if !a.contains('\0')&&!b.contains('\0') {
        #[link(name="shlwapi")]
        unsafe extern "system"{fn StrCmpLogicalW(left:*const u16,right:*const u16)->i32;}
        let left:Vec<u16>=a.encode_utf16().chain(Some(0)).collect();
        let right:Vec<u16>=b.encode_utf16().chain(Some(0)).collect();
        return unsafe{StrCmpLogicalW(left.as_ptr(),right.as_ptr())}.cmp(&0);
    }
    portable_natural_cmp(a,b)
}

fn portable_natural_cmp(a:&str,b:&str)->std::cmp::Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        let ac = ai.peek().copied();
        let bc = bi.peek().copied();
        match (ac, bc) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(a), Some(b)) if a.is_ascii_digit() && b.is_ascii_digit() => {
                // 数字段按数值比较
                let an: String = {
                    let mut s = String::new();
                    while let Some(c) = ai.peek().copied() {
                        if c.is_ascii_digit() {
                            s.push(c);
                            ai.next();
                        } else {
                            break;
                        }
                    }
                    s
                };
                let bn: String = {
                    let mut s = String::new();
                    while let Some(c) = bi.peek().copied() {
                        if c.is_ascii_digit() {
                            s.push(c);
                            bi.next();
                        } else {
                            break;
                        }
                    }
                    s
                };
                let (an, bn) = (an.trim_start_matches('0'), bn.trim_start_matches('0'));
                let ord = an.len().cmp(&bn.len()).then_with(|| an.cmp(bn));
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
            }
            (Some(a), Some(b)) => {
                let ord = a.to_lowercase().cmp(b.to_lowercase());
                if ord != std::cmp::Ordering::Equal {
                    return ord;
                }
                ai.next();
                bi.next();
            }
        }
    }
}

/// 目录请求代号：旧目录请求返回时不得覆盖新标签状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestId(pub u64);

/// 目录枚举结果批次。
#[derive(Debug, Clone)]
pub struct ListingBatch {
    pub request_id: RequestId,
    pub entries: Vec<FileEntry>,
    pub total: usize,
}

/// 可见区请求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleRange {
    pub offset: usize,
    pub length: usize,
}

/// 目录枚举器 trait：平台层实现，领域层只依赖它。
pub trait DirectoryLister: Send {
    /// 枚举目录；`visible` 用于提示优先加载可见区。
    fn list(&self, path: &std::path::Path, visible: VisibleRange) -> Result<ListingBatch>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_first() {
        let mut entries = [
            FileEntry {
                name: "b.txt".into(),
                is_dir: false,
                size: None,
                modified: None,
                created: None,
                identity_key: None,
            },
            FileEntry {
                name: "a-dir".into(),
                is_dir: true,
                size: None,
                modified: None,
                created: None,
                identity_key: None,
            },
        ];
        entries.sort_by(|x, y| {
            y.is_dir
                .cmp(&x.is_dir)
                .then_with(|| natural_cmp(&x.name, &y.name))
        });
        assert_eq!(entries[0].name, "a-dir");
        assert_eq!(entries[1].name, "b.txt");
    }

    #[test]
    fn natural_order_numbers() {
        assert_eq!(
            natural_cmp("file2.txt", "file10.txt"),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            natural_cmp("file10.txt", "file2.txt"),
            std::cmp::Ordering::Greater
        );
        assert_eq!(
            natural_cmp("file2.txt", "file2.txt"),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn natural_order_leading_zeros() {
        assert_eq!(portable_natural_cmp("a01", "a1"), std::cmp::Ordering::Equal);
        assert_eq!(natural_cmp("a01", "a1"),natural_cmp("a1","a01").reverse());
    }
}
