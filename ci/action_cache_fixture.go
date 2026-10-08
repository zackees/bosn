// Builds a real local Git action cache for Bosn retention integration tests.
// Run from the verified Act2 module: go run /path/to/this/file ABSOLUTE_NEW_DIRECTORY.
// Network access is unnecessary. This captures the archive-only baseline;
// the caller must then execute Bosn maintenance and assert the action ceiling.
package main

import (
	"context"
	"crypto/rand"
	"encoding/json"
	"fmt"
	git "github.com/go-git/go-git/v5"
	"github.com/go-git/go-git/v5/plumbing/object"
	"github.com/nektos/act/pkg/artifactcache"
	"github.com/nektos/act/pkg/runner"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"syscall"
	"time"
)

func must(err error) {
	if err != nil {
		panic(err)
	}
}
func allocated(root string) int64 {
	var total int64
	seen := make(map[[2]uint64]bool)
	must(filepath.WalkDir(root, func(p string, d fs.DirEntry, e error) error {
		if e != nil {
			return e
		}
		i, e := d.Info()
		if e != nil {
			return e
		}
		s, ok := i.Sys().(*syscall.Stat_t)
		if !ok {
			return fmt.Errorf("no inode metadata")
		}
		key := [2]uint64{uint64(s.Dev), uint64(s.Ino)}
		if seen[key] {
			return nil
		}
		seen[key] = true
		total += s.Blocks * 512
		return nil
	}))
	return total
}
func main() {
	ctx := context.Background()
	if len(os.Args) != 2 {
		panic("usage: action_cache_fixture ABSOLUTE_NEW_DIRECTORY")
	}
	root := filepath.Clean(os.Args[1])
	if !filepath.IsAbs(root) {
		panic("fixture requires absolute root")
	}
	must(os.Mkdir(root, 0700))
	source := filepath.Join(root, "source")
	repo, e := git.PlainInit(source, false)
	must(e)
	blob := make([]byte, 1024*1024)
	_, e = rand.Read(blob)
	must(e)
	must(os.WriteFile(filepath.Join(source, "payload"), blob, 0600))
	work, e := repo.Worktree()
	must(e)
	_, e = work.Add("payload")
	must(e)
	_, e = work.Commit("real action cache fixture", &git.CommitOptions{Author: &object.Signature{Name: "Bosn fixture", Email: "fixture@example.invalid", When: time.Now()}})
	must(e)
	actions := filepath.Join(root, "cache", "actions")
	cache := runner.GoGitActionCache{Path: actions}
	sha, e := cache.Fetch(ctx, "fixture/actions", "file://"+source, "refs/heads/master", "")
	must(e)
	archive, e := cache.GetTarArchive(ctx, "fixture/actions", sha, "")
	must(e)
	_, e = io.Copy(io.Discard, archive)
	must(e)
	must(archive.Close())
	before := allocated(actions)
	cohort := filepath.Join(root, "cache", "actcache", "cohort-v1")
	policy := artifactcache.Policy{CohortRoot: cohort, CohortMaxBytes: 65536, MaxBytes: 65536, MaxAge: time.Hour, UnusedAge: time.Minute, GCInterval: time.Hour}
	h, e := artifactcache.StartHandlerWithPolicy(filepath.Join(cohort, "0123456789abcdef"), "", "127.0.0.1", 0, nil, policy)
	must(e)
	must(h.Close())
	report := artifactcache.MaintainCohort(ctx, cohort, 65536, policy)
	after := allocated(actions)
	if before <= 65536 || after != before || report.Partial || report.BudgetMet == nil || !*report.BudgetMet {
		panic("fixture did not establish the expected archive-only baseline")
	}
	must(json.NewEncoder(os.Stdout).Encode(struct {
		ActionBefore int64                      `json:"action_before"`
		ActionAfter  int64                      `json:"action_after"`
		ActionBudget int64                      `json:"action_budget"`
		Cohort       artifactcache.CohortReport `json:"cohort"`
	}{before, after, 65536, report}))

}
