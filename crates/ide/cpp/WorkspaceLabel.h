#pragma once
#include <QJsonObject>
#include <QString>
#include <QStringList>

/// How a workspace is named on screen.
///
/// Not by its branch. `bs/agent-4/work` is generated from the agent's own name,
/// so a tab reading "agent-4 . bs/agent-4/work" says one thing twice and never
/// says which repository the agent is in -- which is the question a window
/// holding six agents from three projects actually raises. The branch is still
/// what gets merged, so it keeps its place in the tooltip, where a user who
/// needs it for a git command finds it spelled exactly.
///
/// Header-only and free of Qt widgets, like `Theme.h`, because the four places
/// that show a workspace -- the tab bar, the status bar, the window title and
/// the Explorer's header -- must spell it the same way, and three of them have
/// the strings loose rather than as the tab's JSON. Each function therefore
/// comes in both shapes, with the JSON one a wrapper over the strings one.
namespace workspacelabel {

/// The last component of a repository path: `C:/git/BondSymphonic` ->
/// `BondSymphonic`. What a tab has room for.
///
/// Both separators, because the daemon reports POSIX paths and the New Agent
/// dialog takes Windows ones, and a trailing one is not a component: a path
/// that ends in a slash still names the directory before it.
inline QString repoName(const QString& repoPath) {
    QString normalised = repoPath;
    normalised.replace(QLatin1Char('\\'), QLatin1Char('/'));
    const QStringList parts = normalised.split(QLatin1Char('/'), Qt::SkipEmptyParts);
    return parts.isEmpty() ? repoPath : parts.last();
}

/// `BondSymphonic @ main` -- the repository the work came from and the branch
/// it will go back to. Either half alone when the other is missing, so a tab
/// built from a partial state JSON is still named something.
inline QString origin(const QString& repoPath, const QString& baseBranch) {
    const QString repo = repoName(repoPath);
    if (repo.isEmpty()) {
        return baseBranch;
    }
    return baseBranch.isEmpty() ? repo : repo + QStringLiteral(" @ ") + baseBranch;
}

/// The same with the repository spelled in full, for the status bar and the
/// window title, which have the width for it.
inline QString originFull(const QString& repoPath, const QString& baseBranch) {
    if (repoPath.isEmpty()) {
        return baseBranch;
    }
    return baseBranch.isEmpty() ? repoPath : repoPath + QStringLiteral(" @ ") + baseBranch;
}

/// What a tooltip adds to `origin`: the branch the agent commits to, and the
/// worktree it does the work in. Both are paths or names a user pastes into a
/// shell, so each is on a line of its own and neither is shortened.
inline QString detail(const QString& repoPath, const QString& baseBranch, const QString& branch,
                      const QString& worktreePath) {
    QString out = originFull(repoPath, baseBranch);
    if (!branch.isEmpty()) {
        out += QStringLiteral("\nbranch: ") + branch;
    }
    if (!worktreePath.isEmpty()) {
        out += QStringLiteral("\nworktree: ") + worktreePath;
    }
    return out;
}

/// What an in-place workspace's tooltip adds instead: it has no branch of its
/// own, and the folder it works in is the user's checkout, not a worktree.
inline QString inPlaceDetail(const QString& repoPath, const QString& baseBranch,
                             const QString& worktreePath) {
    return originFull(repoPath, baseBranch) +
           QStringLiteral("\nworks in place: no branch or merge of its own\ncheckout: ") +
           worktreePath;
}

/// What `origin` adds for such a workspace. It has no branch of its own, so
/// the one named is the checkout's, and saying so is what keeps a user from
/// thinking there is something to merge.
inline QString inPlaceSuffix() {
    return QStringLiteral(" (in place)");
}

/// The field names are the daemon's own, as the group model publishes them in
/// its state JSON; a tab that is missing one of them falls through to the
/// empty-half handling above rather than to a placeholder invented here.
inline QString repoPathOf(const QJsonObject& tab) {
    return tab.value(QStringLiteral("repo_path")).toString();
}

/// Whether a tab's workspace works directly in its checkout.
inline bool inPlace(const QJsonObject& tab) {
    return tab.value(QStringLiteral("kind")).toString() == QLatin1String("in_place");
}

inline QString origin(const QJsonObject& tab) {
    const QString out =
        origin(repoPathOf(tab), tab.value(QStringLiteral("base_branch")).toString());
    return inPlace(tab) && !out.isEmpty() ? out + inPlaceSuffix() : out;
}

inline QString originFull(const QJsonObject& tab) {
    const QString out =
        originFull(repoPathOf(tab), tab.value(QStringLiteral("base_branch")).toString());
    return inPlace(tab) && !out.isEmpty() ? out + inPlaceSuffix() : out;
}

inline QString detail(const QJsonObject& tab) {
    if (inPlace(tab)) {
        return inPlaceDetail(repoPathOf(tab), tab.value(QStringLiteral("base_branch")).toString(),
                             tab.value(QStringLiteral("worktree_path")).toString());
    }
    return detail(repoPathOf(tab), tab.value(QStringLiteral("base_branch")).toString(),
                  tab.value(QStringLiteral("branch")).toString(),
                  tab.value(QStringLiteral("worktree_path")).toString());
}

} // namespace workspacelabel
