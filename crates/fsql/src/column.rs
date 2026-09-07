use crate::value::Type;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Table {
    Files,
    Mounts,
    Xattrs,
    Acls,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cost {
    Dirent,
    Statx,
    Readlink,
    Passwd,
    Xattr,
    Acl,
    Content,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Column {
    Path,
    Name,
    Parent,
    Ext,
    Depth,
    Hidden,
    Kind,
    Size,
    Blocks,
    Mode,
    Perms,
    Setuid,
    Setgid,
    Sticky,
    Uid,
    Gid,
    User,
    Group,
    Nlink,
    Inode,
    Dev,
    Atime,
    Mtime,
    Ctime,
    Btime,
    Target,
    Broken,
}

impl Table {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "files" => Some(Self::Files),
            "mounts" => Some(Self::Mounts),
            "xattrs" => Some(Self::Xattrs),
            "acls" => Some(Self::Acls),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Files => "files",
            Self::Mounts => "mounts",
            Self::Xattrs => "xattrs",
            Self::Acls => "acls",
        }
    }

    pub fn columns(self) -> Vec<&'static str> {
        match self {
            Self::Files => Column::ALL.iter().map(|c| c.name()).collect(),
            Self::Mounts => MOUNT_COLUMNS.to_vec(),
            Self::Xattrs => XATTR_COLUMNS.to_vec(),
            Self::Acls => ACL_COLUMNS.to_vec(),
        }
    }
}

pub const MOUNT_COLUMNS: [&str; 12] = [
    "mountpoint",
    "fstype",
    "source",
    "options",
    "readonly",
    "dev",
    "mnt_id",
    "topology",
    "transport",
    "media",
    "case_sensitive",
    "remote",
];

pub const XATTR_COLUMNS: [&str; 4] = ["path", "name", "value", "size"];

pub const ACL_COLUMNS: [&str; 5] = ["path", "kind", "tag", "qualifier", "perms"];

impl Column {
    pub const ALL: [Self; 27] = [
        Self::Path,
        Self::Name,
        Self::Parent,
        Self::Ext,
        Self::Depth,
        Self::Hidden,
        Self::Kind,
        Self::Size,
        Self::Blocks,
        Self::Mode,
        Self::Perms,
        Self::Setuid,
        Self::Setgid,
        Self::Sticky,
        Self::Uid,
        Self::Gid,
        Self::User,
        Self::Group,
        Self::Nlink,
        Self::Inode,
        Self::Dev,
        Self::Atime,
        Self::Mtime,
        Self::Ctime,
        Self::Btime,
        Self::Target,
        Self::Broken,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == name)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Path => "path",
            Self::Name => "name",
            Self::Parent => "parent",
            Self::Ext => "ext",
            Self::Depth => "depth",
            Self::Hidden => "hidden",
            Self::Kind => "kind",
            Self::Size => "size",
            Self::Blocks => "blocks",
            Self::Mode => "mode",
            Self::Perms => "perms",
            Self::Setuid => "setuid",
            Self::Setgid => "setgid",
            Self::Sticky => "sticky",
            Self::Uid => "uid",
            Self::Gid => "gid",
            Self::User => "user",
            Self::Group => "group",
            Self::Nlink => "nlink",
            Self::Inode => "inode",
            Self::Dev => "dev",
            Self::Atime => "atime",
            Self::Mtime => "mtime",
            Self::Ctime => "ctime",
            Self::Btime => "btime",
            Self::Target => "target",
            Self::Broken => "broken",
        }
    }

    pub fn type_of(self) -> Type {
        match self {
            Self::Path
            | Self::Name
            | Self::Parent
            | Self::Ext
            | Self::Kind
            | Self::Perms
            | Self::User
            | Self::Group
            | Self::Target => Type::Text,
            Self::Depth
            | Self::Size
            | Self::Blocks
            | Self::Mode
            | Self::Uid
            | Self::Gid
            | Self::Nlink
            | Self::Inode
            | Self::Dev => Type::Int,
            Self::Hidden | Self::Setuid | Self::Setgid | Self::Sticky | Self::Broken => Type::Bool,
            Self::Atime | Self::Mtime | Self::Ctime | Self::Btime => Type::Timestamp,
        }
    }

    /// Some filesystems report `DT_UNKNOWN` for `kind`, forcing a `statx` fallback.
    pub fn cost(self) -> Cost {
        match self {
            Self::Path | Self::Name | Self::Parent | Self::Ext | Self::Depth | Self::Hidden => {
                Cost::Dirent
            }
            Self::Kind => Cost::Dirent,
            Self::Size
            | Self::Blocks
            | Self::Mode
            | Self::Perms
            | Self::Setuid
            | Self::Setgid
            | Self::Sticky
            | Self::Uid
            | Self::Gid
            | Self::Nlink
            | Self::Inode
            | Self::Dev
            | Self::Atime
            | Self::Mtime
            | Self::Ctime
            | Self::Btime => Cost::Statx,
            Self::User | Self::Group => Cost::Passwd,
            Self::Target | Self::Broken => Cost::Readlink,
        }
    }

    pub fn nullable(self) -> bool {
        matches!(
            self,
            Self::Ext | Self::User | Self::Group | Self::Btime | Self::Target | Self::Broken
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_column_round_trips_through_its_name() {
        for column in Column::ALL {
            assert_eq!(Column::parse(column.name()), Some(column));
        }
    }

    #[test]
    fn column_names_are_unique() {
        let mut names: Vec<&str> = Column::ALL.iter().map(|c| c.name()).collect();
        names.sort_unstable();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count);
    }

    #[test]
    fn unknown_names_do_not_resolve() {
        assert_eq!(Column::parse("sha256"), None);
        assert_eq!(Table::parse("dirs"), None);
    }

    #[test]
    fn path_and_name_need_no_syscall_beyond_the_walk() {
        assert_eq!(Column::Path.cost(), Cost::Dirent);
        assert_eq!(Column::Name.cost(), Cost::Dirent);
        assert_eq!(Column::Size.cost(), Cost::Statx);
        assert_eq!(Column::User.cost(), Cost::Passwd);
    }

    #[test]
    fn mode_derived_columns_share_the_cost_of_mode() {
        for column in [
            Column::Perms,
            Column::Setuid,
            Column::Setgid,
            Column::Sticky,
        ] {
            assert_eq!(column.cost(), Column::Mode.cost());
        }
    }
}
