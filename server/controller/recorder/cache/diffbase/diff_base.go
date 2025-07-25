/*
 * Copyright (c) 2024 Yunshan Networks
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package diffbase

import (
	rcommon "github.com/deepflowio/deepflow/server/controller/recorder/common"
)

// 所有资源的主要信息，用于与cloud数据比较差异，根据差异更新资源
// 应保持字段定义与cloud字段定义一致，用于在比较资源时可以抽象方法
type DataSet struct {
	metadata *rcommon.Metadata

	LogController

	Regions                      map[string]*Region // TODO change to private
	AZs                          map[string]*AZ
	SubDomains                   map[string]*SubDomain
	Hosts                        map[string]*Host
	VMs                          map[string]*VM
	VPCs                         map[string]*VPC
	Networks                     map[string]*Network
	Subnets                      map[string]*Subnet
	VRouters                     map[string]*VRouter
	RoutingTables                map[string]*RoutingTable
	DHCPPorts                    map[string]*DHCPPort
	VInterfaces                  map[string]*VInterface
	WANIPs                       map[string]*WANIP
	LANIPs                       map[string]*LANIP
	FloatingIPs                  map[string]*FloatingIP
	NATGateways                  map[string]*NATGateway
	NATVMConnections             map[string]*NATVMConnection
	NATRules                     map[string]*NATRule
	LBs                          map[string]*LB
	LBVMConnections              map[string]*LBVMConnection
	LBListeners                  map[string]*LBListener
	LBTargetServers              map[string]*LBTargetServer
	PeerConnections              map[string]*PeerConnection
	CENs                         map[string]*CEN
	RDSInstances                 map[string]*RDSInstance
	RedisInstances               map[string]*RedisInstance
	PodClusters                  map[string]*PodCluster
	PodNodes                     map[string]*PodNode
	VMPodNodeConnections         map[string]*VMPodNodeConnection
	PodNamespaces                map[string]*PodNamespace
	PodIngresses                 map[string]*PodIngress
	PodIngressRules              map[string]*PodIngressRule
	PodIngressRuleBackends       map[string]*PodIngressRuleBackend
	PodServices                  map[string]*PodService
	PodServicePorts              map[string]*PodServicePort
	PodGroups                    map[string]*PodGroup
	ConfigMaps                   map[string]*ConfigMap
	PodGroupConfigMapConnections map[string]*PodGroupConfigMapConnection
	PodGroupPorts                map[string]*PodGroupPort
	PodReplicaSets               map[string]*PodReplicaSet
	Pods                         map[string]*Pod
	Process                      map[string]*Process
	VIP                          map[string]*VIP
}

func NewDataSet(md *rcommon.Metadata) *DataSet {
	return &DataSet{
		metadata: md,

		Regions:                      make(map[string]*Region),
		AZs:                          make(map[string]*AZ),
		SubDomains:                   make(map[string]*SubDomain),
		Hosts:                        make(map[string]*Host),
		VMs:                          make(map[string]*VM),
		VPCs:                         make(map[string]*VPC),
		Networks:                     make(map[string]*Network),
		Subnets:                      make(map[string]*Subnet),
		VRouters:                     make(map[string]*VRouter),
		RoutingTables:                make(map[string]*RoutingTable),
		DHCPPorts:                    make(map[string]*DHCPPort),
		VInterfaces:                  make(map[string]*VInterface),
		WANIPs:                       make(map[string]*WANIP),
		LANIPs:                       make(map[string]*LANIP),
		FloatingIPs:                  make(map[string]*FloatingIP),
		NATGateways:                  make(map[string]*NATGateway),
		NATVMConnections:             make(map[string]*NATVMConnection),
		NATRules:                     make(map[string]*NATRule),
		LBs:                          make(map[string]*LB),
		LBVMConnections:              make(map[string]*LBVMConnection),
		LBListeners:                  make(map[string]*LBListener),
		LBTargetServers:              make(map[string]*LBTargetServer),
		PeerConnections:              make(map[string]*PeerConnection),
		CENs:                         make(map[string]*CEN),
		RDSInstances:                 make(map[string]*RDSInstance),
		RedisInstances:               make(map[string]*RedisInstance),
		PodClusters:                  make(map[string]*PodCluster),
		PodNodes:                     make(map[string]*PodNode),
		VMPodNodeConnections:         make(map[string]*VMPodNodeConnection),
		PodNamespaces:                make(map[string]*PodNamespace),
		PodIngresses:                 make(map[string]*PodIngress),
		PodIngressRules:              make(map[string]*PodIngressRule),
		PodIngressRuleBackends:       make(map[string]*PodIngressRuleBackend),
		PodServices:                  make(map[string]*PodService),
		PodServicePorts:              make(map[string]*PodServicePort),
		PodGroups:                    make(map[string]*PodGroup),
		ConfigMaps:                   make(map[string]*ConfigMap),
		PodGroupConfigMapConnections: make(map[string]*PodGroupConfigMapConnection),
		PodGroupPorts:                make(map[string]*PodGroupPort),
		PodReplicaSets:               make(map[string]*PodReplicaSet),
		Pods:                         make(map[string]*Pod),
		Process:                      make(map[string]*Process),
		VIP:                          make(map[string]*VIP),
	}
}

type DiffBase struct {
	Sequence int    `json:"sequence"`
	Lcuuid   string `json:"lcuuid"`
}

func (d *DiffBase) GetSequence() int {
	return d.Sequence
}

func (d *DiffBase) SetSequence(sequence int) {
	d.Sequence = sequence
}

func (d *DiffBase) GetLcuuid() string {
	return d.Lcuuid
}
