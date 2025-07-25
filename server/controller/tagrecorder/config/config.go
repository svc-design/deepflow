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

package config

type TagRecorderConfig struct {
	Interval                  int `default:"60" yaml:"timeout"`
	MySQLBatchSize            int `default:"1000" yaml:"mysql_batch_size"`
	DictionaryRefreshInterval int `default:"60" yaml:"dictionary_refresh_interval"`
	LiveViewRefreshSecond     int `default:"60" yaml:"live_view_refresh_second"`
	DictionaryReloadInterval  int `default:"3600" yaml:"dictionary_reload_interval"`
}
