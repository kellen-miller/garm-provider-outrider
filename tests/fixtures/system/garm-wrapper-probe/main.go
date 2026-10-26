// Local qualification harness. Not Outrider product code or runtime.
package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"path/filepath"
	"time"

	commonParams "github.com/cloudbase/garm-provider-common/params"
	"github.com/cloudbase/garm/config"
	"github.com/cloudbase/garm/params"
	"github.com/cloudbase/garm/runner/common"
	v010 "github.com/cloudbase/garm/runner/providers/v0.1.0"
	"github.com/cloudbase/garm/util"
)

type Options struct {
	Provider   string
	Config     string
	Bootstrap  string
	Controller string
	Output     string
}

type Step struct {
	Operation string `json:"operation"`
	Passed    bool   `json:"passed"`
}

type Report struct {
	SourceCommit string `json:"source_commit"`
	Interface    string `json:"interface"`
	Steps        []Step `json:"steps"`
	Error        string `json:"error,omitempty"`
}

func main() {
	var options Options
	flag.StringVar(&options.Provider, "provider", "", "absolute Rust provider executable")
	flag.StringVar(&options.Config, "config", "", "provider configuration file")
	flag.StringVar(&options.Bootstrap, "bootstrap", "", "bootstrap JSON for prepared local request")
	flag.StringVar(&options.Controller, "controller", "", "configured controller identity")
	flag.StringVar(&options.Output, "output", "", "JSON evidence path")
	flag.Parse()
	report := Report{SourceCommit: "154638445c3949c1958b01812f69d9a1e4d82684", Interface: "v0.1.0"}
	if err := run(options, &report); err != nil {
		report.Error = err.Error()
	}

	data, err := json.MarshalIndent(report, "", "  ")
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}

	data = append(data, '\n')
	if options.Output != "" {
		if err := os.WriteFile(options.Output, data, 0600); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
	}

	os.Stdout.Write(data)
	if report.Error != "" {
		os.Exit(1)
	}
}

func run(options Options, report *Report) error {
	if !filepath.IsAbs(options.Provider) || !filepath.IsAbs(options.Config) || options.Controller == "" || options.Bootstrap == "" {
		return fmt.Errorf("absolute provider/config, controller and bootstrap are required")
	}

	data, err := os.ReadFile(options.Bootstrap)
	if err != nil {
		return err
	}

	var bootstrap commonParams.BootstrapInstance
	if err := json.Unmarshal(data, &bootstrap); err != nil {
		return err
	}

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
	defer cancel()
	bootstrap = util.MaybeAddWrapperToExtraSpecs(ctx, bootstrap)
	cfg := config.Provider{Name: "OutriderProbe", ProviderType: params.ExternalProvider, External: config.External{InterfaceVersion: "v0.1.0", ConfigFile: options.Config, ProviderExecutable: options.Provider, EnvironmentVariables: []string{"OUTRIDER_"}}}
	provider, err := v010.NewProvider(ctx, &cfg, options.Controller)
	if err != nil {
		return err
	}

	instance, err := provider.CreateInstance(ctx, bootstrap, common.CreateInstanceParams{})
	if err != nil {
		return fmt.Errorf("CreateInstance: %w", err)
	}

	report.Steps = append(report.Steps, Step{Operation: "CreateInstance", Passed: true})
	got, err := provider.GetInstance(ctx, instance.ProviderID, common.GetInstanceParams{})
	if err != nil || got.ProviderID != instance.ProviderID {
		return fmt.Errorf("GetInstance identity: %v", err)
	}

	report.Steps = append(report.Steps, Step{Operation: "GetInstance", Passed: true})
	instances, err := provider.ListInstances(ctx, bootstrap.PoolID, common.ListInstancesParams{})
	if err != nil {
		return fmt.Errorf("ListInstances: %w", err)
	}

	found := false
	for _, listed := range instances {
		if listed.ProviderID == instance.ProviderID {
			found = true
		}
	}

	if !found {
		return fmt.Errorf("ListInstances omitted created instance")
	}

	report.Steps = append(report.Steps, Step{Operation: "ListInstances", Passed: true})
	if err := provider.Stop(ctx, instance.ProviderID, common.StopParams{}); err != nil {
		return fmt.Errorf("StopInstance: %w", err)
	}

	report.Steps = append(report.Steps, Step{Operation: "StopInstance", Passed: true})
	if err := provider.Start(ctx, instance.ProviderID, common.StartParams{}); err != nil {
		return fmt.Errorf("StartInstance: %w", err)
	}

	report.Steps = append(report.Steps, Step{Operation: "StartInstance", Passed: true})
	if err := provider.DeleteInstance(ctx, instance.ProviderID, common.DeleteInstanceParams{}); err != nil {
		return fmt.Errorf("DeleteInstance: %w", err)
	}

	report.Steps = append(report.Steps, Step{Operation: "DeleteInstance", Passed: true})
	if err := provider.DeleteInstance(ctx, instance.ProviderID, common.DeleteInstanceParams{}); err != nil {
		return fmt.Errorf("repeat DeleteInstance: %w", err)
	}

	report.Steps = append(report.Steps, Step{Operation: "DeleteInstance repeated", Passed: true})
	if err := provider.RemoveAllInstances(ctx, common.RemoveAllInstancesParams{}); err != nil {
		return fmt.Errorf("RemoveAllInstances: %w", err)
	}

	report.Steps = append(report.Steps, Step{Operation: "RemoveAllInstances", Passed: true})
	return nil
}
