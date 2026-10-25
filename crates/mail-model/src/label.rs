//! Gmail system label ids and the folder constants the app uses.

pub const INBOX: &str = "INBOX";
pub const STARRED: &str = "STARRED";
pub const SENT: &str = "SENT";
pub const DRAFT: &str = "DRAFT";
pub const SPAM: &str = "SPAM";
pub const TRASH: &str = "TRASH";
pub const UNREAD: &str = "UNREAD";
pub const IMPORTANT: &str = "IMPORTANT";
pub const CHAT: &str = "CHAT";
pub const YELLOW_STAR: &str = "YELLOW_STAR";

/// "Primary" is `CATEGORY_PERSONAL` (the search `category:primary` returns nothing via the API).
pub const CATEGORY_PERSONAL: &str = "CATEGORY_PERSONAL";
pub const CATEGORY_UPDATES: &str = "CATEGORY_UPDATES";
pub const CATEGORY_PROMOTIONS: &str = "CATEGORY_PROMOTIONS";
pub const CATEGORY_SOCIAL: &str = "CATEGORY_SOCIAL";
pub const CATEGORY_FORUMS: &str = "CATEGORY_FORUMS";

/// The five category labels in display order.
pub const CATEGORIES: [&str; 5] =
    [CATEGORY_PERSONAL, CATEGORY_UPDATES, CATEGORY_PROMOTIONS, CATEGORY_SOCIAL, CATEGORY_FORUMS];

/// Labels that are never shown as folders.
pub const HIDDEN: [&str; 4] = [UNREAD, IMPORTANT, CHAT, YELLOW_STAR];

/// Folder / label id used for "All Mail": the list call carries no `labelIds` filter.
pub const ALL_MAIL: &str = "ALL";

/// `FolderView.id` of the collapsible "Categories" group (not a label; not clickable).
pub const CATEGORY_GROUP: &str = "group:categories";

/// Prefix of the synthetic ids of user-label grouping nodes (`group:<full path>`).
pub const GROUP_PREFIX: &str = "group:";
